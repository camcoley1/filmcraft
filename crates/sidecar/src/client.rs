//! [`TcpClient`]: the helper over loopback TCP, one request per connection.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};

use serde_json::{Value, json};

use crate::{Hello, JobRequest, JobStatus, Limits, PROTOCOL_VERSION, Result, SidecarClient, SidecarError};

/// The helper at `127.0.0.1:<port>`. Only loopback: the helper reads and writes local files.
#[derive(Clone, Debug)]
pub struct TcpClient {
    addr: SocketAddr,
    limits: Limits,
}

impl TcpClient {
    pub fn new(port: u16, limits: Limits) -> Self {
        Self { addr: SocketAddr::from((Ipv4Addr::LOCALHOST, port)), limits }
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Send one request line and read one reply line (at most [`Limits::max_message`] bytes).
    pub fn call(&self, req: &Value) -> Result<Value> {
        let t = self.limits.io_timeout;
        let mut s = TcpStream::connect_timeout(&self.addr, t).map_err(|e| SidecarError::NotRunning { port: self.addr.port(), msg: e.to_string() })?;
        let io = |e: std::io::Error| SidecarError::Io(e.to_string());
        s.set_read_timeout(Some(t)).map_err(io)?;
        s.set_write_timeout(Some(t)).map_err(io)?;
        let mut line = serde_json::to_vec(req).map_err(|e| SidecarError::Io(e.to_string()))?;
        line.push(b'\n');
        s.write_all(&line).map_err(io)?;
        s.flush().map_err(io)?;
        let cap = self.limits.max_message;
        let mut buf = Vec::new();
        BufReader::new(s.take(cap as u64 + 1)).read_until(b'\n', &mut buf).map_err(io)?;
        if buf.len() > cap {
            return Err(SidecarError::TooLarge(cap));
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
