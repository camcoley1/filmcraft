//! Client for `craft-ai`, the optional local AI helper (`docs/adr/0002-sidecar.md`).
//!
//! The helper is a separate process the user (or the craft-suite launcher) starts; FilmCraft never
//! bundles or spawns it. The two talk over loopback TCP in JSON lines: every request is one line
//! on a fresh connection and gets one line back.
//!
//! | request | reply |
//! |---|---|
//! | `{"op":"hello","protocol":1,"client":str}` | `{"type":"hello","protocol":n,"version":str,"features":[str]}` |
//! | `{"op":"job.start","kind":str,"inputs":[path],"params":{…},"out_dir":path}` | `{"type":"started","job":str}` |
//! | `{"op":"job.status","job":str}` | `{"type":"progress","job":str,"fraction":0..1,"status":str}`, `{"type":"result","job":str,"files":[path],"meta":{…}}` |
//! | `{"op":"job.cancel","job":str}` | `{"type":"cancelled","job":str}` |
//!
//! Any request can be answered with `{"type":"error","message":str}`. Results are files the helper
//! wrote inside `out_dir`; the engine imports them through its own edits, so the helper never
//! writes a project.
//!
//! Everything the helper sends is hostile: replies are capped at [`Limits::max_message`] bytes,
//! result files must lie inside `out_dir` and stay under [`Limits::max_result_bytes`] together,
//! and [`drive`] gives up after [`Limits::timeout`] or when the helper stops answering.
//!
//! Socket code is native only (`cfg(not(target_arch = "wasm32"))`); the protocol types, the
//! [`SidecarClient`] trait and [`drive`] build everywhere.

#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable))]

#[cfg(not(target_arch = "wasm32"))]
mod client;
#[cfg(all(any(test, feature = "fake"), not(target_arch = "wasm32")))]
pub mod fake;

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[cfg(not(target_arch = "wasm32"))]
pub use client::TcpClient;

/// The protocol this build speaks. A helper that answers `hello` with another number is refused.
pub const PROTOCOL_VERSION: u32 = 1;

/// The port `craft-ai` listens on unless told otherwise.
pub const DEFAULT_PORT: u16 = 9877;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SidecarError {
    #[error("AI helper not running on port {port} (start craft-ai: craft-suite/sidecar/run.sh): {msg}")]
    NotRunning { port: u16, msg: String },
    #[error("AI helper is version {version} (protocol {found}); this FilmCraft needs protocol {need}")]
    Version { version: String, found: u32, need: u32 },
    #[error("AI helper connection: {0}")]
    Io(String),
    #[error("AI helper sent a malformed reply: {0}")]
    Malformed(String),
    #[error("AI helper reply is larger than {0} bytes")]
    TooLarge(usize),
    #[error("AI helper: {0}")]
    Helper(String),
    #[error("AI job took longer than {0} s and was stopped")]
    Timeout(u64),
    #[error("stopped")]
    Cancelled,
    #[error("AI job result: {0}")]
    BadResult(String),
}

pub type Result<T> = std::result::Result<T, SidecarError>;

/// The helper's answer to `hello`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
    /// Job kinds the helper can run (`echo`, `reframe`, `stems`…).
    #[serde(default)]
    pub features: Vec<String>,
}

impl Hello {
    /// `Ok` when the helper speaks [`PROTOCOL_VERSION`].
    pub fn check(&self) -> Result<()> {
        if self.protocol == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(SidecarError::Version { version: self.version.clone(), found: self.protocol, need: PROTOCOL_VERSION })
        }
    }

    pub fn has(&self, feature: &str) -> bool {
        self.features.iter().any(|f| f == feature)
    }
}

/// One job for the helper.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobRequest {
    pub kind: String,
    /// Media files the job reads.
    pub inputs: Vec<String>,
    pub params: Value,
    /// Where the helper writes its result files (a folder of the project's AI cache).
    pub out_dir: String,
}

/// What the helper wrote.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobResult {
    pub files: Vec<String>,
    #[serde(default)]
    pub meta: Value,
}

/// A job's state as last reported.
#[derive(Clone, Debug, PartialEq)]
pub enum JobStatus {
    Running { fraction: f64, status: String },
    Done(JobResult),
}

/// Talks to a helper. [`TcpClient`] is the real one; tests install fakes.
pub trait SidecarClient: Send + Sync {
    /// The handshake: what the helper is and can do (not yet checked against our protocol).
    fn hello(&self) -> Result<Hello>;
    /// Start a job; returns the helper's job id.
    fn start(&self, req: &JobRequest) -> Result<String>;
    /// Progress or the result of a job. A failed job is an `Err`.
    fn status(&self, job: &str) -> Result<JobStatus>;
    /// Ask the helper to stop a job (it may still finish; its result is then ignored).
    fn cancel(&self, job: &str) -> Result<()>;
}

/// Caps on what a job may cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Limits {
    /// Longest reply line accepted.
    pub max_message: usize,
    /// Result files: total size.
    pub max_result_bytes: u64,
    /// Result files: count.
    pub max_files: usize,
    /// Wall-clock time a job may take.
    pub timeout: Duration,
    /// Time between status requests.
    pub poll_interval: Duration,
    /// Status requests that may fail in a row (a busy helper) before the job is given up.
    pub max_poll_failures: u32,
    /// Connect / read / write timeout of one request.
    pub io_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message: 1 << 20,
            max_result_bytes: 16 << 30,
            max_files: 256,
            timeout: Duration::from_secs(4 * 3600),
            poll_interval: Duration::from_millis(250),
            max_poll_failures: 8,
            io_timeout: Duration::from_secs(5),
        }
    }
}

/// The request line for `op` (without the newline).
pub fn request(op: &str, body: Value) -> Value {
    let mut v = json!({"op": op});
    if let (Some(o), Value::Object(b)) = (v.as_object_mut(), body) {
        o.extend(b);
    }
    v
}

/// Parse one reply line: an object with a `type`; `{"type":"error"}` becomes [`SidecarError::Helper`].
pub fn parse_reply(line: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| SidecarError::Malformed(e.to_string()))?;
    let Some(kind) = v.get("type").and_then(Value::as_str) else {
        return Err(SidecarError::Malformed("reply has no \"type\"".into()));
    };
    if kind == "error" {
        let msg = v.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(SidecarError::Helper(msg.chars().take(500).collect()));
    }
    Ok(v)
}

fn expect_type(v: &Value, want: &str) -> Result<()> {
    match v.get("type").and_then(Value::as_str) {
        Some(t) if t == want => Ok(()),
        t => Err(SidecarError::Malformed(format!("expected a \"{want}\" reply, got {t:?}"))),
    }
}

/// Decode a `hello` reply.
pub fn hello_from(v: Value) -> Result<Hello> {
    expect_type(&v, "hello")?;
    serde_json::from_value(v).map_err(|e| SidecarError::Malformed(format!("hello: {e}")))
}

/// Decode a `started` reply into the job id.
pub fn started_from(v: &Value) -> Result<String> {
    expect_type(v, "started")?;
    let id = v.get("job").and_then(Value::as_str).filter(|s| !s.is_empty() && s.len() <= 128);
    id.map(str::to_string).ok_or_else(|| SidecarError::Malformed("started: no job id".into()))
}

/// Decode a `job.status` reply.
pub fn status_from(v: Value) -> Result<JobStatus> {
    match v.get("type").and_then(Value::as_str) {
        Some("progress") => {
            let f = v.get("fraction").and_then(Value::as_f64).filter(|f| f.is_finite()).unwrap_or(0.0).clamp(0.0, 1.0);
            let status = v.get("status").and_then(Value::as_str).unwrap_or("").chars().take(200).collect();
            Ok(JobStatus::Running { fraction: f, status })
        }
        Some("result") => serde_json::from_value(v).map(JobStatus::Done).map_err(|e| SidecarError::Malformed(format!("result: {e}"))),
        t => Err(SidecarError::Malformed(format!("expected progress or result, got {t:?}"))),
    }
}

/// Run `req` to the end: start it, poll its progress (`on_progress(fraction, status)`), cancel it
/// when `cancelled()` turns true or [`Limits::timeout`] passes, and check its result files with
/// [`check_result`]. A helper that stops answering for [`Limits::max_poll_failures`] polls in a
/// row (killed, hung) fails the job.
pub fn drive(
    client: &dyn SidecarClient,
    req: &JobRequest,
    limits: &Limits,
    cancelled: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(f64, &str),
) -> Result<(JobResult, Vec<PathBuf>)> {
    let t0 = std::time::Instant::now();
    let job = client.start(req)?;
    let mut failures = 0u32;
    loop {
        if cancelled() {
            let _ = client.cancel(&job);
            return Err(SidecarError::Cancelled);
        }
        if t0.elapsed() > limits.timeout {
            let _ = client.cancel(&job);
            return Err(SidecarError::Timeout(limits.timeout.as_secs()));
        }
        match client.status(&job) {
            Ok(JobStatus::Running { fraction, status }) => {
                failures = 0;
                on_progress(fraction, &status);
            }
            Ok(JobStatus::Done(r)) => {
                on_progress(1.0, "done");
                let files = check_result(&r, Path::new(&req.out_dir), limits)?;
                return Ok((r, files));
            }
            // the helper reported the job failed: that is final
            Err(e @ SidecarError::Helper(_)) => return Err(e),
            Err(e) => {
                failures += 1;
                if failures >= limits.max_poll_failures.max(1) {
                    return Err(SidecarError::Io(format!("the helper stopped answering ({e})")));
                }
            }
        }
        std::thread::sleep(limits.poll_interval);
    }
}

/// The result's files, resolved (relative names are inside `out_dir`), each an existing regular
/// file inside `out_dir`, together within [`Limits::max_result_bytes`].
pub fn check_result(r: &JobResult, out_dir: &Path, limits: &Limits) -> Result<Vec<PathBuf>> {
    if r.files.len() > limits.max_files {
        return Err(SidecarError::BadResult(format!("{} files (at most {})", r.files.len(), limits.max_files)));
    }
    let root = out_dir.canonicalize().map_err(|e| SidecarError::BadResult(format!("{}: {e}", out_dir.display())))?;
    let mut total = 0u64;
    let mut out = Vec::with_capacity(r.files.len());
    for f in &r.files {
        let p = Path::new(f);
        let p = if p.is_absolute() { p.to_path_buf() } else { root.join(p) };
        let real = p.canonicalize().map_err(|e| SidecarError::BadResult(format!("{f}: missing ({e})")))?;
        if !real.starts_with(&root) {
            return Err(SidecarError::BadResult(format!("{f} is outside the job's folder")));
        }
        let m = std::fs::metadata(&real).map_err(|e| SidecarError::BadResult(format!("{f}: {e}")))?;
        if !m.is_file() {
            return Err(SidecarError::BadResult(format!("{f} is not a file")));
        }
        total = total.saturating_add(m.len());
        if total > limits.max_result_bytes {
            return Err(SidecarError::BadResult(format!("result files are larger than {} bytes", limits.max_result_bytes)));
        }
        out.push(real);
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
