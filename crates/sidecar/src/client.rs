//! [`TcpClient`]: the helper over loopback TCP, one request per connection.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::{Hello, JobRequest, JobStatus, Limits, PROTOCOL_VERSION, Result, SidecarClient, SidecarError};

/// The helper at `127.0.0.1:<port>`. Only loopback: the helper reads and writes local files.
#[derive(Clone, Debug)]
pub struct TcpClient {
    addr: SocketAddr,
    limits: Limits,
    /// The token sent with every request; `None` = read [`crate::token_file`] at each request
    /// (a restarted helper writes a new one).
    token: Option<String>,
}

impl TcpClient {
    pub fn new(port: u16, limits: Limits) -> Self {
        Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), limits, token: None }
    }

    /// Send this token instead of reading the token file (tests, hosts that know it).
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    fn token(&self) -> Option<String> {
        self.token.clone().or_else(|| crate::token_file().and_then(|f| crate::read_token(&f)))
    }

    /// Send one request line and read one reply line (at most [`Limits::max_message`] bytes),
    /// all within [`Limits::io_timeout`] in total: a silent or trickling peer fails at the
    /// deadline instead of holding the caller.
    pub fn call(&self, req: &Value) -> Result<Value> {
        let t = self.limits.io_timeout;
        let deadline = Instant::now() + t;
        let left = || deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
        let late = || SidecarError::Io(format!("no reply within {:.1} s", t.as_secs_f64()));
        let s = TcpStream::connect_timeout(&self.addr, t).map_err(|e| SidecarError::NotRunning { port: self.addr.port(), msg: e.to_string() })?;
        let io = |e: std::io::Error| match e.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => late(),
            _ => SidecarError::Io(e.to_string()),
        };
        let mut req = req.clone();
        if let (Some(o), Some(tok)) = (req.as_object_mut(), self.token()) {
            o.insert("token".into(), Value::String(tok));
        }
        let mut line = serde_json::to_vec(&req).map_err(|e| SidecarError::Io(e.to_string()))?;
        line.push(b'\n');
        s.set_write_timeout(Some(left().ok_or_else(late)?)).map_err(io)?;
        (&s).write_all(&line).map_err(io)?;
        let cap = self.limits.max_message;
        let mut r = BufReader::new(&s);
        let mut buf = Vec::new();
        loop {
            let rem: Duration = left().ok_or_else(late)?;
            r.get_ref().set_read_timeout(Some(rem)).map_err(io)?;
            let chunk = r.fill_buf().map_err(io)?;
            if chunk.is_empty() {
                break;
            }
            let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            buf.extend_from_slice(chunk.get(..take).unwrap_or_default());
            r.consume(take);
            if buf.len() > cap {
                return Err(SidecarError::TooLarge(cap));
            }
            if done {
                break;
            }
        }
        if buf.is_empty() {
            return Err(SidecarError::Io("the helper closed the connection without a reply".into()));
        }
        let text = std::str::from_utf8(&buf).map_err(|e| SidecarError::Malformed(e.to_string()))?;
        crate::parse_reply(text)
    }
}

impl SidecarClient for TcpClient {
    fn hello(&self) -> Result<Hello> {
        let client = format!("filmcraft {}", env!("CARGO_PKG_VERSION"));
        crate::hello_from(self.call(&crate::request("hello", json!({"protocol": PROTOCOL_VERSION, "client": client})))?)
    }

    fn start(&self, req: &JobRequest) -> Result<String> {
        let body = serde_json::to_value(req).map_err(|e| SidecarError::Io(e.to_string()))?;
        crate::started_from(&self.call(&crate::request("job.start", body))?)
    }

    fn status(&self, job: &str) -> Result<JobStatus> {
        crate::status_from(self.call(&crate::request("job.status", json!({"job": job})))?)
    }

    fn cancel(&self, job: &str) -> Result<()> {
        self.call(&crate::request("job.cancel", json!({"job": job}))).map(|_| ())
    }
}
