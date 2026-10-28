use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::fake::{FakeHelper, Malformed, ResultMode, Script};

fn limits() -> Limits {
    Limits { poll_interval: Duration::from_millis(5), max_poll_failures: 3, io_timeout: Duration::from_secs(2), ..Default::default() }
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("filmcraft-sidecar-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn req(dir: &Path) -> JobRequest {
    JobRequest { kind: "echo".into(), inputs: vec!["/media/a.mov".into()], params: json!({"x": 1}), out_dir: dir.to_string_lossy().into() }
}

fn run(script: Script, name: &str) -> (Result<(JobResult, Vec<PathBuf>)>, FakeHelper) {
    run_with(script, name, limits(), &|| false)
}

fn run_with(script: Script, name: &str, l: Limits, cancelled: &dyn Fn() -> bool) -> (Result<(JobResult, Vec<PathBuf>)>, FakeHelper) {
    let h = FakeHelper::start(script).unwrap();
    let c = TcpClient::new(h.port(), l);
    let dir = out_dir(name);
    let mut seen = Vec::new();
    let r = drive(&c, &req(&dir), &l, cancelled, &mut |f, _| seen.push(f));
    assert!(seen.windows(2).all(|w| w[0] <= w[1]), "progress only grows: {seen:?}");
    (r, h)
}

#[test]
fn hello_handshake_and_version_check() {
    let h = FakeHelper::start(Script::default()).unwrap();
    let hello = TcpClient::new(h.port(), limits()).hello().unwrap();
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert!(hello.has("echo") && !hello.has("reframe"));
    hello.check().unwrap();

    let h = FakeHelper::start(Script { protocol: 99, version: "9.9".into(), ..Default::default() }).unwrap();
    let e = TcpClient::new(h.port(), limits()).hello().unwrap().check().unwrap_err();
    assert_eq!(e, SidecarError::Version { version: "9.9".into(), found: 99, need: PROTOCOL_VERSION });
    assert!(e.to_string().contains("version 9.9"), "{e}");
}

#[test]
fn no_helper_is_not_running() {
    // a port nobody listens on: bind one, then free it
    let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let e = TcpClient::new(port, limits()).hello().unwrap_err();
    assert!(matches!(e, SidecarError::NotRunning { .. }), "{e:?}");
    assert!(e.to_string().contains("craft-ai"), "{e}");
}

#[test]
fn echo_job_succeeds() {
    let (r, h) = run(Script::default(), "ok");
    let (res, files) = r.unwrap();
    assert_eq!(res.meta, json!({"x": 1}));
    assert_eq!(files.len(), 1);
    let back: JobRequest = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(back.inputs, ["/media/a.mov"]);
    assert_eq!(h.started().len(), 1);
}

#[test]
fn cancel_mid_job_tells_the_helper() {
    let polls = std::sync::atomic::AtomicU32::new(0);
    let cancelled = || polls.fetch_add(1, Ordering::Relaxed) >= 2;
    let (r, h) = run_with(Script { steps: 1000, ..Default::default() }, "cancel", limits(), &cancelled);
    assert_eq!(r.unwrap_err(), SidecarError::Cancelled);
    assert_eq!(h.cancelled(), ["j1"]);
}

#[test]
fn malformed_replies_are_errors() {
    let h = FakeHelper::start(Script { malformed: Malformed::Hello, ..Default::default() }).unwrap();
    assert!(matches!(TcpClient::new(h.port(), limits()).hello(), Err(SidecarError::Malformed(_))));
    // every status reply is garbage: the job is given up after max_poll_failures
    let (r, _h) = run(Script { malformed: Malformed::Status, ..Default::default() }, "malformed");
    assert!(matches!(r, Err(SidecarError::Io(ref m)) if m.contains("stopped answering")), "{r:?}");
    for bad in ["", "{}", "[1]", "{\"type\":3}", "nul"] {
        assert!(parse_reply(bad).is_err(), "{bad:?}");
    }
    assert!(matches!(parse_reply(r#"{"type":"error","message":"boom"}"#), Err(SidecarError::Helper(m)) if m == "boom"));
    assert!(status_from(json!({"type": "result", "files": "nope"})).is_err());
    let s = status_from(json!({"type": "progress", "fraction": f64::MAX, "status": 7})).unwrap();
    assert_eq!(s, JobStatus::Running { fraction: 1.0, status: String::new() });
}

#[test]
fn oversized_reply_line_is_refused() {
    let l = Limits { max_message: 16, ..limits() };
    let h = FakeHelper::start(Script { features: vec!["a-very-long-feature-name".into()], ..Default::default() }).unwrap();
    assert_eq!(TcpClient::new(h.port(), l).hello().unwrap_err(), SidecarError::TooLarge(16));
}

#[test]
fn bad_result_files_are_refused() {
    let (r, _h) = run(Script { result: ResultMode::Missing, ..Default::default() }, "missing");
    assert!(matches!(r, Err(SidecarError::BadResult(ref m)) if m.contains("missing")), "{r:?}");
    let l = Limits { max_result_bytes: 1000, ..limits() };
    let (r, _h) = run_with(Script { result: ResultMode::Oversize(1001), ..Default::default() }, "big", l, &|| false);
    assert!(matches!(r, Err(SidecarError::BadResult(ref m)) if m.contains("larger")), "{r:?}");
    let (r, _h) = run(Script { result: ResultMode::Outside, ..Default::default() }, "outside");
    assert!(matches!(r, Err(SidecarError::BadResult(ref m)) if m.contains("outside")), "{r:?}");
    let (r, _h) = run(Script { result: ResultMode::Fail("no GPU".into()), ..Default::default() }, "fail");
    assert_eq!(r.unwrap_err(), SidecarError::Helper("no GPU".into()));
    let too_many = JobResult { files: vec!["a".into(); 3], meta: Value::Null };
    assert!(check_result(&too_many, Path::new("/"), &Limits { max_files: 2, ..limits() }).is_err());
}

#[test]
fn helper_killed_mid_job_fails_the_job() {
    let (r, _h) = run(Script { steps: 1000, die_at_poll: Some(3), ..Default::default() }, "killed");
    assert!(matches!(r, Err(SidecarError::Io(ref m)) if m.contains("stopped answering")), "{r:?}");
}

#[test]
fn job_timeout_cancels() {
    let l = Limits { timeout: Duration::from_millis(30), ..limits() };
    let stop = AtomicBool::new(false);
    let (r, h) = run_with(Script { steps: 100_000, ..Default::default() }, "timeout", l, &|| stop.load(Ordering::Relaxed));
    assert!(matches!(r, Err(SidecarError::Timeout(_))), "{r:?}");
    assert_eq!(h.cancelled(), ["j1"]);
}

#[test]
fn silent_and_trickling_peers_fail_at_the_deadline() {
    for mode in [Malformed::Silent, Malformed::Trickle] {
        let h = FakeHelper::start(Script { malformed: mode, ..Default::default() }).unwrap();
        let l = Limits { io_timeout: Duration::from_millis(200), ..limits() };
        let t0 = std::time::Instant::now();
        let e = TcpClient::new(h.port(), l).hello().unwrap_err();
        let took = t0.elapsed();
        assert!(matches!(e, SidecarError::Io(ref m) if m.contains("no reply within")), "{mode:?}: {e:?}");
        assert!(took < Duration::from_millis(1500), "{mode:?} held the caller for {took:?}");
    }
}

#[test]
fn the_token_is_required_when_the_helper_has_one() {
    let h = FakeHelper::start(Script { token: Some("s3cret".into()), ..Default::default() }).unwrap();
    let e = TcpClient::new(h.port(), limits()).with_token("wrong").hello().unwrap_err();
    assert!(matches!(e, SidecarError::Helper(ref m) if m.contains("unauthorized")), "{e:?}");
    TcpClient::new(h.port(), limits()).with_token("s3cret").hello().unwrap();
    // the token file: first line, trimmed, bounded
    let d = out_dir("token");
    let f = d.join("token");
    std::fs::write(&f, "abc123\nignored").unwrap();
    assert_eq!(read_token(&f).as_deref(), Some("abc123"));
    std::fs::write(&f, "x".repeat(10_000)).unwrap();
    assert_eq!(read_token(&f).map(|t| t.len()), Some(256));
    std::fs::write(&f, "  \n").unwrap();
    assert_eq!(read_token(&f), None);
    assert_eq!(read_token(&d.join("none")), None);
}

#[cfg(unix)]
#[test]
fn symlinked_or_swapped_results_are_refused() {
    let d = out_dir("links");
    let outside = d.parent().unwrap().join(format!("filmcraft-sidecar-{}-target.bin", std::process::id()));
    std::fs::write(&outside, b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, d.join("link.bin")).unwrap();
    let r = JobResult { files: vec!["link.bin".into()], meta: Value::Null };
    assert!(matches!(check_result(&r, &d, &limits()), Err(SidecarError::BadResult(ref m)) if m.contains("symbolic link") || m.contains("outside")));
    std::fs::write(d.join("ok.bin"), b"data").unwrap();
    let ok = check_result(&JobResult { files: vec!["ok.bin".into()], meta: Value::Null }, &d, &limits()).unwrap();
    assert!(open_result(&ok[0], &d, 100).is_ok());
    assert!(open_result(&ok[0], &d, 2).is_err(), "over the size cap");
    // swapped for a link after the check
    std::fs::remove_file(&ok[0]).unwrap();
    std::os::unix::fs::symlink(&outside, &ok[0]).unwrap();
    assert!(open_result(&ok[0], &d, 100).is_err());
}
