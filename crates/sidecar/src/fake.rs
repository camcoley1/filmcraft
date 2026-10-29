//! A scripted in-process helper for tests (feature `fake`): a real loopback TCP server speaking
//! the protocol, whose replies a [`Script`] bends into the failures a real helper can produce.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::{JobRequest, PROTOCOL_VERSION};

/// Files a scripted job writes: `(name in out_dir, bytes)`.
pub type Writer = fn(&JobRequest) -> Vec<(String, Vec<u8>)>;

/// What the result of a finished job is.
#[derive(Clone, Debug)]
pub enum ResultMode {
    /// `echo.json` (the request) in `out_dir`; `meta` = the job's params.
    Echo,
    /// Names a file that was never written.
    Missing,
    /// A file of this many bytes.
    Oversize(u64),
    /// A file outside `out_dir`.
    Outside,
    /// The job fails with this message.
    Fail(String),
    /// These files, made from the request (a feature's result format).
    Custom(Writer),
}

/// Which reply is not JSON, or how the helper misbehaves on the wire.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Malformed {
    None,
    Hello,
    Status,
    /// Reads the request and never answers (a hung helper).
    Silent,
    /// Answers one byte at a time, never ending the line.
    Trickle,
}

#[derive(Clone, Debug)]
pub struct Script {
    pub protocol: u32,
    pub version: String,
    pub features: Vec<String>,
    /// Status polls answered with progress before the result.
    pub steps: u32,
    pub result: ResultMode,
    pub malformed: Malformed,
    /// The helper dies (stops listening, drops the connection) at this status poll.
    pub die_at_poll: Option<u32>,
    /// Requests must carry this token (`None`: any or none).
    pub token: Option<String>,
    /// The `hello` reply waits this long (a busy helper).
    pub hello_delay: Duration,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            version: "0.1.0-fake".into(),
            features: vec!["echo".into()],
            steps: 2,
            result: ResultMode::Echo,
            malformed: Malformed::None,
            die_at_poll: None,
            token: None,
            hello_delay: Duration::ZERO,
        }
    }
}

#[derive(Default)]
struct State {
    hellos: u32,
    started: Vec<JobRequest>,
    polls: u32,
    cancelled: Vec<String>,
}

pub struct FakeHelper {
    port: u16,
    stop: Arc<AtomicBool>,
    state: Arc<Mutex<State>>,
    thread: Option<JoinHandle<()>>,
}

impl FakeHelper {
    pub fn start(script: Script) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State::default()));
        let (st, sp) = (state.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !sp.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => serve(s, &script, &st, &sp),
                    Err(_) => std::thread::sleep(Duration::from_millis(2)),
                }
            }
        });
        Ok(Self { port, stop, state, thread: Some(thread) })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stop listening now (a killed helper): later connections are refused.
    pub fn kill(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    pub fn started(&self) -> Vec<JobRequest> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).started.clone()
    }

    /// How many `hello` requests arrived.
    pub fn hellos(&self) -> u32 {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).hellos
    }

    pub fn cancelled(&self) -> Vec<String> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).cancelled.clone()
    }
}

impl Drop for FakeHelper {
    fn drop(&mut self) {
        self.kill();
    }
}

fn serve(s: TcpStream, script: &Script, state: &Mutex<State>, stop: &AtomicBool) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let mut r = BufReader::new(&s);
    if r.read_line(&mut line).is_err() {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let mut w = &s;
    match script.malformed {
        Malformed::Silent => {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            return;
        }
        Malformed::Trickle => {
            while !stop.load(Ordering::Relaxed) && w.write_all(b" ").is_ok() {
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        _ => {}
    }
    if let Some(t) = &script.token
        && req["token"].as_str() != Some(t.as_str())
    {
        let _ = w.write_all(b"{\"type\":\"error\",\"message\":\"unauthorized: wrong or missing token\"}\n");
        return;
    }
    let reply = match reply(&req, script, state, stop) {
        Some(r) => r,
        None => return,
    };
    let _ = w.write_all(reply.as_bytes());
    let _ = w.write_all(b"\n");
}

/// The reply line; None = drop the connection without one.
fn reply(req: &Value, script: &Script, state: &Mutex<State>, stop: &AtomicBool) -> Option<String> {
    let op = req["op"].as_str().unwrap_or("");
    if op == "hello" {
        state.lock().unwrap_or_else(PoisonError::into_inner).hellos += 1;
        std::thread::sleep(script.hello_delay);
    }
    let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
    let v = match op {
        "hello" if script.malformed == Malformed::Hello => return Some("{not json".into()),
        "hello" => json!({"type": "hello", "protocol": script.protocol, "version": script.version, "features": script.features}),
        "job.start" => match serde_json::from_value::<JobRequest>(req.clone()) {
            Ok(j) => {
                st.started.push(j);
                json!({"type": "started", "job": format!("j{}", st.started.len())})
            }
            Err(e) => json!({"type": "error", "message": e.to_string()}),
        },
        "job.status" => {
            st.polls += 1;
            if script.die_at_poll.is_some_and(|n| st.polls >= n) {
                stop.store(true, Ordering::Relaxed);
                return None;
            }
            if script.malformed == Malformed::Status {
                return Some("]]garbage".into());
            }
            let n = req["job"].as_str().and_then(|j| j.strip_prefix('j')).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
            let Some(job) = n.checked_sub(1).and_then(|i| st.started.get(i)).cloned() else {
                return Some(json!({"type": "error", "message": "no such job"}).to_string());
            };
            if st.polls <= script.steps {
                json!({"type": "progress", "job": req["job"], "fraction": st.polls as f64 / (script.steps + 1) as f64, "status": "working"})
            } else {
                finish(&job, &req["job"], &script.result)
            }
        }
        "job.cancel" => {
            st.cancelled.push(req["job"].as_str().unwrap_or("").to_string());
            json!({"type": "cancelled", "job": req["job"]})
        }
        _ => json!({"type": "error", "message": format!("unknown op {op:?}")}),
    };
    Some(v.to_string())
}

fn finish(job: &JobRequest, id: &Value, mode: &ResultMode) -> Value {
    let dir = std::path::Path::new(&job.out_dir);
    let _ = std::fs::create_dir_all(dir);
    let file = match mode {
        ResultMode::Fail(m) => return json!({"type": "error", "job": id, "message": m}),
        ResultMode::Custom(write) => {
            let names: Vec<String> = write(job)
                .into_iter()
                .map(|(name, bytes)| {
                    let _ = std::fs::write(dir.join(&name), bytes);
                    name
                })
                .collect();
            return json!({"type": "result", "job": id, "files": names, "meta": job.params});
        }
        ResultMode::Echo => {
            let path = dir.join("echo.json");
            let _ = std::fs::write(&path, serde_json::to_vec(job).unwrap_or_default());
            "echo.json".to_string()
        }
        ResultMode::Missing => "missing.bin".to_string(),
        ResultMode::Oversize(n) => {
            let path = dir.join("big.bin");
            if let Ok(f) = std::fs::File::create(&path) {
                let _ = f.set_len(*n);
            }
            "big.bin".to_string()
        }
        ResultMode::Outside => {
            let path = dir.parent().unwrap_or(dir).join("outside.bin");
            let _ = std::fs::write(&path, b"x");
            path.to_string_lossy().to_string()
        }
    };
    json!({"type": "result", "job": id, "files": [file], "meta": job.params})
}
