//! Executed native-worker journeys using a loopback scripted provider only.
mod support;
#[path = "../src/protocol.rs"]
#[allow(dead_code)]
mod wire_contract;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

fn frame(line: &str) -> Value {
    let value: Value = serde_json::from_str(line).expect("stdout must contain complete protocol frames only");
    let event: wire_contract::Event = serde_json::from_value(value.clone()).expect("worker frame envelope");
    event.validate_payload().expect("real worker payload must match generated wire schema");
    value
}

struct Worker {
    child: Child,
    input: ChildStdin,
    events: Receiver<Value>,
    diagnostics: std::path::PathBuf,
}
impl Worker {
    fn start(root: &std::path::Path, home: &std::path::Path, url: &str, extra: &[&str]) -> Self {
        Self::start_fault(root, home, url, extra, None)
    }
    fn start_fault(
        root: &std::path::Path,
        home: &std::path::Path,
        url: &str,
        extra: &[&str],
        fault: Option<&str>,
    ) -> Self {
        let diagnostics = home.join("worker-test-diagnostics.txt");
        let mut command = Command::new(support::RUSTY);
        command
            .args([
                "--session-worker",
                "--protocol-stdio",
                "--memory",
                "off",
                "--mode",
                "standard",
                "--context-tokens",
                "128000",
            ])
            .args(extra)
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home)
            .env("RUSTY_HOME", home)
            .env("RUSTY_NO_DOTENV", "1")
            .env("NVIDIA_API_KEY", "offline-fixture-value")
            .env("NVIDIA_BASE_URL", url)
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_ALLOW_DESTRUCTIVE", "1")
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(std::fs::File::create(&diagnostics).unwrap()));
        if !extra.contains(&"--agents") {
            command.args(["--agents", "off"]);
        }
        if let Some(point) = fault {
            if point == "inherited_sigint_ignore" {
                use std::os::unix::process::CommandExt;
                unsafe {
                    command.pre_exec(|| {
                        if libc::signal(libc::SIGINT, libc::SIG_IGN) == libc::SIG_ERR {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            } else if point == "approval_expiry" {
                command.env("RUSTY_WORKER_TEST_APPROVAL_TTL_MS", "50");
            } else {
                command.env("RUSTY_WORKER_ABORT_AT", point);
            }
        }
        let mut child = command.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                let event = frame(&line);
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        let w = Self { child, input, events, diagnostics };
        w.until("hello");
        w
    }
    fn send(&mut self, id: &str, method: &str, params: Value) {
        writeln!(self.input, "{}", json!({"protocol":1,"request_id":id,"method":method,"params":params})).unwrap();
        self.input.flush().unwrap();
    }
    fn until(&self, kind: &str) -> Value {
        let start = Instant::now();
        let mut recent = Vec::new();
        loop {
            let e = self.events.recv_timeout(Duration::from_secs(45)).unwrap_or_else(|e| {
                panic!(
                    "worker event {kind}: {e:?}; recent={recent:?}; diagnostics={}",
                    std::fs::read_to_string(&self.diagnostics).unwrap_or_default()
                )
            });
            if e["kind"] == kind {
                return e;
            }
            if recent.len() >= 12 {
                recent.remove(0);
            }
            recent.push(e);
            assert!(start.elapsed() < Duration::from_secs(55), "missing {kind}: {recent:?}");
        }
    }
    fn response(&self, id: &str) -> Value {
        loop {
            let e = self.until("reply");
            if e["payload"]["request_id"] == id {
                return e["payload"].clone();
            }
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn worker_preserves_the_launchers_ignored_terminal_interrupts() {
    let tmp = support::TempDir::new("native-ignored-interrupt");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut worker = Worker::start_fault(&root, &home, "http://127.0.0.1:9/v1", &[], Some("inherited_sigint_ignore"));
    worker.until("session.ready");
    // A native launcher owns terminal signal disposition. In particular the
    // CLI's first/second Ctrl-C handler must not replace inherited SIG_IGN.
    for index in 0..3 {
        assert_eq!(unsafe { libc::kill(worker.child.id() as i32, libc::SIGINT) }, 0);
        std::thread::sleep(Duration::from_millis(100));
        assert!(worker.child.try_wait().unwrap().is_none(), "terminal handler ended the native worker");
        let id = format!("after-interrupt-{index}");
        worker.send(&id, "snapshot", json!({}));
        assert_eq!(worker.response(&id)["result"]["phase"], "idle");
    }
    worker.send("stop", "shutdown", json!({}));
    assert_eq!(worker.response("stop")["ok"], true);
    worker.until("worker.closing");
    assert!(worker.child.wait().unwrap().success());
}
fn text(s: &str) -> Value {
    json!({"choices":[{"delta":{"content":s},"finish_reason":"stop"}]})
}
fn tool(name: &str, args: Value) -> Value {
    json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
}
fn provider(replies: Vec<Value>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let count = replies.len();
    let mut replies = replies.into_iter();
    provider_fn(count, move |_| replies.next().unwrap())
}
fn provider_fn(
    count: usize,
    mut respond: impl FnMut(&Value) -> Value + Send + 'static,
) -> (String, std::thread::JoinHandle<Vec<Value>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let join = std::thread::spawn(move || {
        let mut seen = vec![];
        for _ in 0..count {
            let start = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(55));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((k, v)) = line.split_once(':') {
                    if k.eq_ignore_ascii_case("content-length") {
                        length = v.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request = serde_json::from_slice(&body).unwrap();
            let value = respond(&request);
            seen.push(request);
            let data = format!("data: {value}\n\ndata: [DONE]\n\n");
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",data.len()).unwrap();
        }
        seen
    });
    (url, join)
}
#[test]
fn approval_round_trip_edit_receipts_and_digest_validation() {
    let tmp = support::TempDir::new("native-approval");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("calc.txt", "before\n")]);
    let (url, server) =
        provider(vec![tool("write_file", json!({"path":"calc.txt","content":"after\n"})), text("saved")]);
    let mut w = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
    w.send("submit-1", "message.submit", json!({"text":"update calc"}));
    let approval = w.until("approval.requested")["payload"].clone();
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "before\n");
    w.send(
        "bad",
        "approval.decide",
        json!({"approval_id":approval["approval_id"],"operation_digest":"wrong","allowed":true}),
    );
    assert_eq!(w.response("bad")["ok"], false);
    w.send(
        "yes",
        "approval.decide",
        json!({"approval_id":approval["approval_id"],"operation_digest":approval["operation_digest"],"allowed":true}),
    );
    assert_eq!(w.until("run.completed")["payload"]["state"], "completed");
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "after\n");
    w.send(
        "duplicate-decision",
        "approval.decide",
        json!({"approval_id":approval["approval_id"],"operation_digest":approval["operation_digest"],"allowed":true}),
    );
    assert_eq!(w.response("duplicate-decision")["ok"], false);
    w.send("submit-1", "message.submit", json!({"text":"update calc"}));
    assert_eq!(w.response("submit-1")["result"]["duplicate"], true);
    w.send("submit-1", "message.submit", json!({"text":"different"}));
    assert_eq!(w.response("submit-1")["ok"], false);
    assert_eq!(server.join().unwrap().len(), 2);
}
#[test]
fn yolo_worker_never_honors_unattended_destructive_env() {
    let tmp = support::TempDir::new("native-destructive");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "valuable\n")]);
    std::fs::write(root.join("keep-untracked.txt"), "valuable").unwrap();
    let (url, server) = provider(vec![tool("bash", json!({"command":"git clean -fdx"})), text("declined")]);
    let trajectory = tmp.path().join("decline-trajectory.json");
    let mut w =
        Worker::start(&root, &home, &url, &["--permissions", "yolo", "--trajectory", trajectory.to_str().unwrap()]);
    w.send("danger", "message.submit", json!({"text":"remove file"}));
    let approval = w.until("approval.requested")["payload"].clone();
    assert_eq!(approval["typed_phrase"], "yes");
    w.send("short","approval.decide",json!({"approval_id":approval["approval_id"],"operation_digest":approval["operation_digest"],"allowed":true,"typed":"y"}));
    assert_eq!(w.response("short")["ok"], false);
    w.send(
        "deny",
        "approval.decide",
        json!({"approval_id":approval["approval_id"],"operation_digest":approval["operation_digest"],"allowed":false}),
    );
    w.until("run.completed");
    assert!(root.join("keep.txt").exists());
    assert!(root.join("keep-untracked.txt").exists());
    assert_eq!(server.join().unwrap().len(), 2);
    let trajectory: Value = serde_json::from_slice(&std::fs::read(trajectory).unwrap()).unwrap();
    assert_eq!(trajectory["safety"]["destructive"]["declined"], 1);
    assert_eq!(trajectory["safety"]["destructive"]["unattended"], 0);
}
#[test]
fn inspect_needs_no_provider_and_refuses_escaping_path() {
    let tmp = support::TempDir::new("native-inspect");
    support::git_repo(tmp.path(), &[("hello.txt", "hello\n")]);
    let mut child = Command::new(support::RUSTY)
        .args(["--inspect-worker", "--protocol-stdio"])
        .current_dir(tmp.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for (id, path) in [("file", "hello.txt"), ("escape", "../outside")] {
            writeln!(input,"{}",json!({"protocol":1,"request_id":id,"method":"inspect","params":{"operation":"file.read","arguments":{"path":path}}})).unwrap();
        }
    }
    let output = support::wait(child, Duration::from_secs(10));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let rows: Vec<Value> = String::from_utf8(output.stdout).unwrap().lines().map(frame).collect();
    assert_eq!(rows[0]["kind"], "hello");
    assert_eq!(rows[1]["payload"]["ok"], true);
    assert_eq!(rows[2]["payload"]["ok"], false);
}

#[test]
fn crash_recovery_retains_uncertain_receipt_and_releases_lease() {
    let tmp = support::TempDir::new("native-recovery");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("calc.txt", "before\n")]);
    let (url, server) = provider(vec![tool("write_file", json!({"path":"calc.txt","content":"never\n"}))]);
    let mut w = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
    let initial = w.until("session.snapshot")["payload"].clone();
    let id = initial["session_id"].as_str().unwrap().to_string();
    w.send("durable", "message.submit", json!({"text":"update calc"}));
    w.until("approval.requested");
    // A later queue rejection must not hide the still-running run on recovery.
    for n in 0..16 {
        let request = format!("queued-{n}");
        w.send(&request, "message.submit", json!({"text":"queued work"}));
        assert_eq!(w.response(&request)["ok"], true);
    }
    w.send("queue-overflow", "message.submit", json!({"text":"not admitted"}));
    assert_eq!(w.response("queue-overflow")["ok"], false);
    // A second client never steals the existing writer's flock.
    let competing = Command::new(support::RUSTY)
        .args([
            "--session-worker",
            "--protocol-stdio",
            "--continue",
            &id,
            "--memory",
            "off",
            "--context-tokens",
            "128000",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("RUSTY_HOME", &home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("NVIDIA_API_KEY", "offline-fixture-value")
        .env("RUSTY_BASE_URL", &url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = support::wait(competing, Duration::from_secs(10));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("already open for writing"));
    let startup: Vec<_> = String::from_utf8(output.stdout).unwrap().lines().map(frame).collect();
    assert_eq!(startup[0]["kind"], "hello");
    assert!(startup.iter().any(|event| event["kind"] == "startup.failed" && event["payload"]["code"] == "lease_busy"));
    drop(w);
    assert_eq!(server.join().unwrap().len(), 1);
    let mut resumed = Worker::start(&root, &home, &url, &["--continue", &id]);
    let recovered = resumed.until("session.snapshot")["payload"].clone();
    assert_eq!(recovered["phase"], "uncertain");
    assert_eq!(recovered["uncertain_run_count"], 17);
    assert!(recovered["uncertain_runs"].as_array().unwrap().iter().any(|r| r["request_id"] == "durable"));
    assert!(recovered["history"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| { m["content"].as_str().is_some_and(|text| text.contains("uncertain external effects")) }));
    resumed.send("queue-overflow", "message.submit", json!({"text":"not admitted"}));
    assert_eq!(resumed.response("queue-overflow")["result"]["receipt"]["state"], "not_started");
    resumed.send("durable", "message.submit", json!({"text":"update calc"}));
    let receipt = resumed.response("durable");
    assert_eq!(receipt["result"]["duplicate"], true);
    assert_eq!(receipt["result"]["receipt"]["state"], "uncertain");
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "before\n");
}

#[test]
fn cancel_is_bound_to_run_and_does_not_execute_pending_approval() {
    let tmp = support::TempDir::new("native-cancel");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("calc.txt", "before\n")]);
    let (url, server) = provider(vec![tool("write_file", json!({"path":"calc.txt","content":"never\n"}))]);
    let mut w = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
    w.send("submit", "message.submit", json!({"text":"update calc"}));
    let approval = w.until("approval.requested")["payload"].clone();
    w.send("stale", "run.cancel", json!({"run_id":"wrong"}));
    assert_eq!(w.response("stale")["ok"], false);
    w.send("cancel", "run.cancel", json!({"run_id":approval["operation"]["run_id"]}));
    assert_eq!(w.until("run.completed")["payload"]["state"], "interrupted");
    assert_eq!(std::fs::read_to_string(root.join("calc.txt")).unwrap(), "before\n");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn dedicated_fd_keeps_diagnostics_out_of_frames() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    let tmp = support::TempDir::new("native-fd");
    support::git_repo(tmp.path(), &[("hello.txt", "hello\n")]);
    let (mut parent, socket) = UnixStream::pair().unwrap();
    parent.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let fd = socket.as_raw_fd();
    let mut command = Command::new(support::RUSTY);
    command
        .args(["--inspect-worker"])
        .current_dir(tmp.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::fcntl(3, libc::F_SETFD, 0);
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    drop(socket);
    writeln!(parent,"{}",json!({"protocol":1,"request_id":"fd-file","method":"inspect","params":{"operation":"file.read","arguments":{"path":"hello.txt"}}})).unwrap();
    parent.shutdown(std::net::Shutdown::Write).unwrap();
    let mut frames = String::new();
    parent.read_to_string(&mut frames).unwrap();
    let rows: Vec<Value> = frames.lines().map(frame).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["payload"]["ok"], true);
    let output = support::wait(child, Duration::from_secs(5));
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn kill_during_admitted_tool_does_not_replay_its_effect() {
    let tmp = support::TempDir::new("native-effect");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) =
        provider(vec![tool("bash", json!({"command":"echo $$ > child.pid; printf one >> effect.txt; sleep 30"}))]);
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "yolo"]);
    let initial = worker.until("session.snapshot")["payload"].clone();
    let id = initial["session_id"].as_str().unwrap().to_string();
    worker.send("effect", "message.submit", json!({"text":"record one local effect"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("effect.txt").exists() {
        assert!(Instant::now() < deadline, "tool effect never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let process_group: libc::pid_t = std::fs::read_to_string(root.join("child.pid")).unwrap().trim().parse().unwrap();
    drop(worker);
    // SIGKILL of the worker cannot run its child cleanup; reap only this
    // fixture's explicitly recorded process group before leaving the test.
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    server.join().unwrap();
    let mut resumed = Worker::start(&root, &home, &url, &["--continue", &id]);
    let recovered = resumed.until("session.snapshot")["payload"].clone();
    assert_eq!(recovered["uncertain_tool_count"], 1);
    assert_eq!(recovered["uncertain_tools"][0]["tool"], "bash");
    assert!(recovered["uncertain_tools"][0]["summary"].as_str().unwrap().contains("effect.txt"));
    resumed.send("effect", "message.submit", json!({"text":"record one local effect"}));
    assert_eq!(resumed.response("effect")["result"]["receipt"]["state"], "uncertain");
    assert_eq!(std::fs::read_to_string(root.join("effect.txt")).unwrap(), "one");
}

#[test]
fn delegated_worker_text_is_not_streamed_as_the_leads_answer() {
    let tmp = support::TempDir::new("native-worker-events");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider(vec![
        tool("task", json!({"description":"inspect","prompt":"give a short finding"})),
        text("CHILD_ONLY"),
        text("LEAD_ANSWER"),
    ]);
    let mut worker = Worker::start(&root, &home, &url, &["--agents", "sub"]);
    worker.send("delegate", "message.submit", json!({"text":"delegate a small inspection"}));
    let mut answer = String::new();
    loop {
        let event = worker.events.recv_timeout(Duration::from_secs(30)).unwrap();
        if event["kind"] == "message.delta" {
            answer.push_str(event["payload"]["text"].as_str().unwrap());
        }
        if event["kind"] == "run.completed" {
            assert_eq!(event["payload"]["state"], "completed");
            break;
        }
    }
    assert!(answer.contains("LEAD_ANSWER"), "{answer}");
    assert!(!answer.contains("CHILD_ONLY"), "{answer}");
    assert_eq!(server.join().unwrap().len(), 3);
}

#[cfg(debug_assertions)]
fn fault_recovery(point: &str, expected: Option<&str>) {
    let tmp = support::TempDir::new("native-killpoint");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let needs_provider = matches!(point, "mid_stream" | "before_final_receipt");
    let (url, server) = if needs_provider {
        let (url, join) = provider(vec![text("partial response")]);
        (url, Some(join))
    } else {
        ("http://127.0.0.1:9/v1".into(), None)
    };
    let mut worker = Worker::start_fault(&root, &home, &url, &[], Some(point));
    let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
    worker.send("fault-request", "message.submit", json!({"text":"do fixture work"}));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = worker.child.try_wait().unwrap() {
            assert!(!status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Some(server) = server {
        assert_eq!(server.join().unwrap().len(), 1);
    }
    drop(worker);
    let mut resumed = Worker::start(&root, &home, &url, &["--continue", &id]);
    let snapshot = resumed.until("session.snapshot")["payload"].clone();
    match expected {
        Some(state) => {
            assert_eq!(snapshot["receipts"]["fault-request"]["state"], state);
            resumed.send("fault-request", "message.submit", json!({"text":"do fixture work"}));
            assert_eq!(resumed.response("fault-request")["result"]["duplicate"], true);
        }
        None => assert!(snapshot["receipts"].get("fault-request").is_none()),
    }
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "before\n");
}
#[test]
#[cfg(debug_assertions)]
fn kill_before_acceptance_has_no_receipt() {
    fault_recovery("before_acceptance", None);
}
#[test]
#[cfg(debug_assertions)]
fn kill_after_receipt_never_replays() {
    fault_recovery("after_receipt", Some("uncertain"));
}
#[test]
#[cfg(debug_assertions)]
fn kill_before_reply_never_replays() {
    fault_recovery("before_reply", Some("uncertain"));
}
#[test]
#[cfg(debug_assertions)]
fn kill_mid_stream_never_replays() {
    fault_recovery("mid_stream", Some("uncertain"));
}
#[test]
#[cfg(debug_assertions)]
fn kill_before_final_receipt_never_replays() {
    fault_recovery("before_final_receipt", Some("uncertain"));
}

#[test]
fn clean_shutdown_marks_queued_work_not_started_and_drains_final_frames() {
    let tmp = support::TempDir::new("native-shutdown");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider(vec![tool("write_file", json!({"path":"keep.txt","content":"never"}))]);
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
    let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
    worker.send("running", "message.submit", json!({"text":"update keep"}));
    worker.until("approval.requested");
    worker.send("queued", "message.submit", json!({"text":"queued work"}));
    assert_eq!(worker.response("queued")["ok"], true);
    worker.send("shutdown", "shutdown", json!({}));
    let mut seen = Vec::new();
    loop {
        let event = worker.events.recv_timeout(Duration::from_secs(10)).unwrap();
        seen.push(event["kind"].as_str().unwrap().to_string());
        if event["kind"] == "worker.closing" {
            break;
        }
    }
    assert!(
        seen.contains(&"run.completed".into())
            && seen.contains(&"session.snapshot".into())
            && seen.contains(&"reply".into())
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = worker.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    server.join().unwrap();
    drop(worker);
    let mut resumed = Worker::start(&root, &home, &url, &["--continue", &id]);
    let snapshot = resumed.until("session.snapshot")["payload"].clone();
    assert_eq!(snapshot["receipts"]["queued"]["state"], "not_started");
    resumed.send("queued", "message.submit", json!({"text":"queued work"}));
    assert_eq!(resumed.response("queued")["result"]["duplicate"], true);
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "before\n");
}

#[test]
fn cancel_mid_bash_stops_the_owned_child() {
    let tmp = support::TempDir::new("native-cancel-bash");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider(vec![tool("bash", json!({"command":"echo $$ > child.pid; exec sleep 30"}))]);
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "yolo"]);
    worker.send("run", "message.submit", json!({"text":"start the cancellable fixture"}));
    let run = worker.until("run.started")["payload"]["run_id"].clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("child.pid").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid: libc::pid_t = std::fs::read_to_string(root.join("child.pid")).unwrap().trim().parse().unwrap();
    worker.send("cancel", "run.cancel", json!({"run_id":run}));
    let completion = worker.until("run.completed");
    assert_eq!(completion["payload"]["state"], "interrupted");
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "owned child remained alive after cancellation");
    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn cancel_mid_model_stream_preserves_partial_text_without_another_request() {
    let tmp = support::TempDir::new("native-cancel-stream");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let (release, hold) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap();
                }
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {}\n\n",
            json!({"choices":[{"delta":{"content":"partial"}}]})
        )
        .unwrap();
        stream.flush().unwrap();
        hold.recv_timeout(Duration::from_secs(10)).unwrap();
    });
    let mut worker = Worker::start(&root, &home, &url, &[]);
    worker.send("run", "message.submit", json!({"text":"stream a fixture answer"}));
    let run = worker.until("run.started")["payload"]["run_id"].clone();
    assert_eq!(worker.until("message.delta")["payload"]["text"], "partial");
    worker.send("cancel", "run.cancel", json!({"run_id":run}));
    assert_eq!(worker.until("run.completed")["payload"]["state"], "interrupted");
    worker.send("snapshot", "snapshot", json!({}));
    let snapshot = worker.response("snapshot")["result"].clone();
    assert_eq!(snapshot["live_text"], "partial");
    release.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn a_slow_reader_drops_events_then_resynchronizes_without_ending_session() {
    let tmp = support::TempDir::new("native-slow-reader");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let mut child = Command::new(support::RUSTY)
        .args([
            "--session-worker",
            "--protocol-stdio",
            "--memory",
            "off",
            "--agents",
            "off",
            "--context-tokens",
            "128000",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("RUSTY_HOME", &home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("NVIDIA_API_KEY", "offline-fixture-value")
        .env("RUSTY_BASE_URL", "http://127.0.0.1:9/v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for n in 0..2000 {
        writeln!(
            input,
            "{}",
            json!({"protocol":1,"request_id":format!("snapshot-{n}"),"method":"snapshot","params":{}})
        )
        .unwrap();
    }
    input.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(child.try_wait().unwrap().is_none(), "backpressure ended the session");
    let output = child.stdout.take().unwrap();
    let (tx, events) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            tx.send(frame(&line.unwrap())).unwrap();
        }
    });
    // Let the already queued burst drain; a fresh snapshot then carries all
    // durable state even though replies and deltas were dropped under load.
    let mut saw_resync = false;
    let quiet = Duration::from_millis(200);
    while let Ok(event) = events.recv_timeout(quiet) {
        saw_resync |= event["kind"] == "resync_required";
    }
    writeln!(input, "{}", json!({"protocol":1,"request_id":"fresh","method":"snapshot","params":{}})).unwrap();
    input.flush().unwrap();
    loop {
        let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
        saw_resync |= event["kind"] == "resync_required";
        if event["kind"] == "reply" && event["payload"]["request_id"] == "fresh" {
            assert_eq!(event["payload"]["result"]["phase"], "idle");
            break;
        }
    }
    assert!(saw_resync);
    writeln!(input, "{}", json!({"protocol":1,"request_id":"stop","method":"shutdown","params":{}})).unwrap();
    input.flush().unwrap();
    drop(input);
    while events.recv_timeout(Duration::from_secs(5)).is_ok() {}
    let output = support::wait(child, Duration::from_secs(5));
    assert!(output.status.success());
    reader.join().unwrap();
}

#[test]
fn oversized_legacy_receipt_ledger_opens_read_only_without_replay() {
    let tmp = support::TempDir::new("native-oversized-ledger");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let url = "http://127.0.0.1:9/v1";
    let mut worker = Worker::start(&root, &home, url, &[]);
    let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
    worker.send("stop", "shutdown", json!({}));
    worker.until("worker.closing");
    drop(worker);
    fn find(path: &std::path::Path) -> std::path::PathBuf {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                let found = find(&path);
                if !found.as_os_str().is_empty() {
                    return found;
                }
            } else if path.to_string_lossy().ends_with(".worker.json") {
                return path;
            }
        }
        std::path::PathBuf::new()
    }
    let ledger = find(&home);
    assert!(!ledger.as_os_str().is_empty());
    let bytes = vec![b' '; 8 * 1024 * 1024 + 1];
    std::fs::write(&ledger, &bytes).unwrap();
    let mut resumed = Worker::start(&root, &home, url, &["--continue", &id]);
    let snapshot = resumed.until("session.snapshot")["payload"].clone();
    assert_eq!(snapshot["read_only"], true);
    assert_eq!(snapshot["recovery_unknown"], true);
    assert_eq!(snapshot["read_only_reason"], "receipt_ledger_over_limit");
    resumed.send("submit", "message.submit", json!({"text":"must never run"}));
    assert_eq!(resumed.response("submit")["ok"], false);
    assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
    resumed.send("stop", "shutdown", json!({}));
    assert_eq!(resumed.response("stop")["ok"], true);
    drop(resumed);
    // The decoded cap also applies to tiny compressed streams; the original
    // bytes remain untouched and no receipt can be guessed or replayed.
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(&bytes).unwrap();
    let compressed = gzip.finish().unwrap();
    assert!(compressed.len() < 64 * 1024);
    std::fs::write(&ledger, &compressed).unwrap();
    let mut inflated = Worker::start(&root, &home, url, &["--continue", &id]);
    let snapshot = inflated.until("session.snapshot")["payload"].clone();
    assert_eq!(snapshot["read_only"], true);
    assert_eq!(snapshot["read_only_reason"], "receipt_ledger_over_limit");
    inflated.send("submit", "message.submit", json!({"text":"must never run"}));
    assert_eq!(inflated.response("submit")["ok"], false);
    assert_eq!(std::fs::read(&ledger).unwrap(), compressed);
    inflated.send("stop", "shutdown", json!({}));
    assert_eq!(inflated.response("stop")["ok"], true);
    drop(inflated);
    // Corruption has a typed startup failure, rather than an empty channel.
    std::fs::write(&ledger, b"not json").unwrap();
    let invalid = Worker::start(&root, &home, url, &["--continue", &id]);
    assert_eq!(invalid.until("startup.failed")["payload"]["code"], "receipt_invalid");
    drop(invalid);
    std::fs::write(&ledger, [0x1f, 0x8b, 0, 0]).unwrap();
    let invalid_gzip = Worker::start(&root, &home, url, &["--continue", &id]);
    assert_eq!(invalid_gzip.until("startup.failed")["payload"]["code"], "receipt_invalid");
}

#[test]
fn session_fd3_preserves_frames_redirects_diagnostics_and_keeps_key_out_of_arguments() {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;
    let tmp = support::TempDir::new("native-session-fd");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) =
        provider(vec![tool("write_file", json!({"path":"keep.txt","content":"after\n"})), text("done")]);
    let (mut channel, socket) = UnixStream::pair().unwrap();
    channel.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let fd = socket.as_raw_fd();
    let mut command = Command::new(support::RUSTY);
    command
        .args([
            "--session-worker",
            "--memory",
            "off",
            "--agents",
            "off",
            "--permissions",
            "ask",
            "--context-tokens",
            "128000",
        ])
        .current_dir(&root)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("RUSTY_HOME", &home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("NVIDIA_API_KEY", "offline-fixture-value")
        .env("RUSTY_BASE_URL", &url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::fcntl(3, libc::F_SETFD, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(socket);
    let args = Command::new("/bin/ps").args(["-p", &child.id().to_string(), "-o", "command="]).output().unwrap();
    assert!(args.status.success());
    assert!(!String::from_utf8_lossy(&args.stdout).contains("offline-fixture-value"));
    if let Err(error) = child.stdin.take().unwrap().write_all(b"unrelated terminal input\n") {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe, "unexpected stdin write error: {error}");
    }
    let mut reader = BufReader::new(channel.try_clone().unwrap());
    let mut frames = 0;
    let mut next = || {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        frames += 1;
        frame(&line)
    };
    assert_eq!(next()["kind"], "hello");
    loop {
        if next()["kind"] == "session.snapshot" {
            break;
        }
    }
    writeln!(
        channel,
        "{}",
        json!({"protocol":1,"request_id":"submit","method":"message.submit","params":{"text":"update keep"}})
    )
    .unwrap();
    let approval = loop {
        let event = next();
        if event["kind"] == "approval.requested" {
            break event["payload"].clone();
        }
    };
    writeln!(channel,"{}",json!({"protocol":1,"request_id":"allow","method":"approval.decide","params":{"approval_id":approval["approval_id"],"operation_digest":approval["operation_digest"],"allowed":true}})).unwrap();
    loop {
        if next()["kind"] == "run.completed" {
            break;
        }
    }
    writeln!(channel, "{}", json!({"protocol":1,"request_id":"stop","method":"shutdown","params":{}})).unwrap();
    loop {
        if next()["kind"] == "worker.closing" {
            break;
        }
    }
    drop(next);
    let mut tail = String::new();
    reader.read_to_string(&mut tail).unwrap();
    assert!(tail.is_empty());
    assert!(frames > 10);
    let output = support::wait(child, Duration::from_secs(5));
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn fifty_cancel_shutdown_journeys_drain_completion_snapshot_and_reply() {
    for n in 0..50 {
        let tmp = support::TempDir::new("native-drain-repeat");
        let root = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        support::git_repo(&root, &[("keep.txt", "before\n")]);
        let (url, server) = provider(vec![tool("write_file", json!({"path":"keep.txt","content":"never"}))]);
        let mut worker = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
        worker.send("submit", "message.submit", json!({"text":"update keep"}));
        let approval = worker.until("approval.requested");
        worker.send("cancel", "run.cancel", json!({"run_id":approval["payload"]["operation"]["run_id"]}));
        worker.send("stop", "shutdown", json!({}));
        let mut completion = false;
        let mut snapshot = false;
        let mut shutdown = false;
        loop {
            let event = worker.events.recv_timeout(Duration::from_secs(10)).unwrap();
            match event["kind"].as_str().unwrap() {
                "run.completed" => {
                    completion = true;
                    assert_eq!(event["payload"]["state"], "interrupted");
                }
                "session.snapshot" => snapshot = true,
                "reply" if event["payload"]["request_id"] == "stop" => shutdown = true,
                "worker.closing" => break,
                _ => {}
            }
        }
        assert!(completion && snapshot && shutdown, "missing final frames in iteration {n}");
        server.join().unwrap();
    }
}

#[test]
fn parallel_read_only_swarm_tools_keep_distinct_requester_identity() {
    let tmp = support::TempDir::new("native-parallel-tools");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let mut lead_started = false;
    let (url, server) = provider_fn(6, move |request| {
        let messages = request["messages"].as_array().unwrap();
        let worker = messages.iter().any(|message| {
            message["content"]
                .as_str()
                .is_some_and(|content| content.contains("INSPECT_A") || content.contains("INSPECT_B"))
        });
        // Worker requests carry their task as the final user message; the lead
        // only receives those strings inside its own serialized tool arguments.
        let worker = worker
            && messages.iter().any(|message| {
                message["role"] == "user"
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.contains("INSPECT_A") || content.contains("INSPECT_B"))
            });
        if worker {
            if messages.iter().any(|message| message["role"] == "tool") {
                text("worker inspected")
            } else {
                tool("read_file", json!({"path":"keep.txt"}))
            }
        } else if !lead_started {
            lead_started = true;
            tool(
                "swarm",
                json!({"tasks":[{"description":"reader-a","prompt":"INSPECT_A read keep.txt"},{"description":"reader-b","prompt":"INSPECT_B read keep.txt"}]}),
            )
        } else {
            text("lead complete")
        }
    });
    let mut worker = Worker::start(&root, &home, &url, &["--agents", "sub"]);
    worker.send("run", "message.submit", json!({"text":"delegate two separate inspections"}));
    let mut requesters = Vec::new();
    loop {
        let event = worker.events.recv_timeout(Duration::from_secs(20)).unwrap();
        if event["kind"] == "tool.started" && event["payload"]["name"] == "read_file" {
            requesters.push(event["payload"]["requester"].as_str().unwrap().to_string());
        }
        if event["kind"] == "run.completed" {
            assert_eq!(event["payload"]["state"], "completed");
            break;
        }
    }
    requesters.sort();
    assert_eq!(requesters, ["reader-a", "reader-b"]);
    assert_eq!(server.join().unwrap().len(), 6);
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "before\n");
}

#[test]
#[cfg(debug_assertions)]
fn approval_expiry_is_distinct_from_user_decline_and_never_executes() {
    let tmp = support::TempDir::new("native-expiry");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) =
        provider(vec![tool("write_file", json!({"path":"keep.txt","content":"never"})), text("authorization expired")]);
    let mut worker = Worker::start_fault(&root, &home, &url, &["--permissions", "ask"], Some("approval_expiry"));
    worker.send("run", "message.submit", json!({"text":"update keep"}));
    worker.until("approval.requested");
    let expired = worker.until("approval.expired");
    assert_eq!(expired["payload"]["reason"], "approval expired");
    assert_eq!(worker.until("run.completed")["payload"]["state"], "completed");
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "before\n");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let tool_message = requests[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap()
        ["content"]
        .as_str()
        .unwrap();
    assert!(tool_message.contains("approval expired"));
    assert!(!tool_message.contains("the user declined"));
}

#[test]
fn crash_after_background_handle_retains_tool_until_actual_process_ends() {
    let tmp = support::TempDir::new("native-activity-crash");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider(vec![tool(
        "bash_start",
        json!({"command":"echo $$ > child.pid; printf one > activity-effect.txt; sleep 30","timeout_secs":60}),
    )]);
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "yolo"]);
    let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
    worker.send("background", "message.submit", json!({"text":"start an owned background activity"}));
    loop {
        let event = worker.until("tool.completed");
        if event["payload"]["name"] == "bash_start" {
            assert_eq!(event["payload"]["ok"], true);
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("activity-effect.txt").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let process_group: libc::pid_t = std::fs::read_to_string(root.join("child.pid")).unwrap().trim().parse().unwrap();
    drop(worker);
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    assert_eq!(server.join().unwrap().len(), 1);
    let mut resumed = Worker::start(&root, &home, &url, &["--continue", &id]);
    let snapshot = resumed.until("session.snapshot")["payload"].clone();
    assert_eq!(snapshot["uncertain_tool_count"], 1);
    assert_eq!(snapshot["uncertain_tools"][0]["tool"], "bash_start");
    assert!(snapshot["uncertain_tools"][0]["activity_id"].as_str().unwrap().starts_with("shell-"));
    resumed.send("background", "message.submit", json!({"text":"start an owned background activity"}));
    assert_eq!(resumed.response("background")["result"]["duplicate"], true);
    assert_eq!(std::fs::read_to_string(root.join("activity-effect.txt")).unwrap(), "one");
}

#[test]
fn completed_background_activity_clears_its_durable_flight() {
    let tmp = support::TempDir::new("native-activity-finish");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider_fn(3, |request| {
        let results: Vec<_> = request["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
        match results.len() {
            0 => tool("bash_start", json!({"command":"sleep 0.1; printf one > activity-effect.txt","timeout_secs":30})),
            1 => {
                let record: Value = serde_json::from_str(results[0]["content"].as_str().unwrap()).unwrap();
                tool("bash_wait", json!({"id":record["id"],"wait_secs":5}))
            }
            _ => text("activity verified"),
        }
    });
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "yolo"]);
    worker.send("background", "message.submit", json!({"text":"start, wait for and verify an activity"}));
    assert_eq!(worker.until("run.completed")["payload"]["state"], "completed");
    let snapshot = worker.until("session.snapshot")["payload"].clone();
    assert_eq!(snapshot["in_flight_tools"], json!([]));
    assert_eq!(snapshot["uncertain_tool_count"], 0);
    assert_eq!(server.join().unwrap().len(), 3);
    assert_eq!(std::fs::read_to_string(root.join("activity-effect.txt")).unwrap(), "one");
}

// Replace only this test's pipe endpoint so truncated input reaches EOF. The
// Worker helper keeps a valid, inert ChildStdin for its existing Drop behavior;
// no unsafe close/double-close or second process is needed.
fn close_worker_input(worker: &mut Worker) {
    let null: std::os::fd::OwnedFd = std::fs::OpenOptions::new().write(true).open("/dev/null").unwrap().into();
    drop(std::mem::replace(&mut worker.input, ChildStdin::from(null)));
}

fn assert_protocol_error_closes(worker: &mut Worker, expect_completion: bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut error, mut snapshot, mut completion) = (false, false, false);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "protocol shutdown deadline elapsed");
        let event = worker.events.recv_timeout(remaining).unwrap();
        match event["kind"].as_str().unwrap() {
            "protocol.error" => error = true,
            "session.snapshot" => snapshot = true,
            "run.completed" => {
                assert_eq!(event["payload"]["state"], "interrupted");
                completion = true;
            }
            "worker.closing" => {
                assert_eq!(event["payload"]["reason"], "protocol_error");
                break;
            }
            _ => (),
        }
    }
    assert!(error && snapshot, "protocol error suppressed its final state");
    assert_eq!(completion, expect_completion);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = worker.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(74));
            break;
        }
        assert!(Instant::now() < deadline, "protocol-error worker did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_protocol_session_lease_released(root: &std::path::Path, home: &std::path::Path, url: &str, id: &str) {
    let mut resumed = Worker::start(root, home, url, &["--continue", id]);
    resumed.until("session.ready");
    resumed.send("stop", "shutdown", json!({}));
    resumed.until("worker.closing");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = resumed.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "resumed worker did not release its lease");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn invalid_input_drains_final_state_and_exits_nonzero_without_provider_calls() {
    for (label, bytes) in [
        ("malformed", "{broken JSON}\n"),
        ("unknown", "{\"protocol\":1,\"request_id\":\"bad\",\"method\":\"unknown.control\",\"params\":{}}\n"),
        ("truncated", "{\"protocol\":1,\"request_id\":\"partial\""),
    ] {
        let tmp = support::TempDir::new(&format!("native-input-{label}"));
        let root = tmp.path().join("project");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        support::git_repo(&root, &[("keep.txt", "before\n")]);
        // Any attempted provider connection remains observable in this local
        // listener's accept queue; no server response or paid endpoint exists.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let mut worker = Worker::start(&root, &home, &url, &[]);
        let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
        worker.input.write_all(bytes.as_bytes()).unwrap();
        worker.input.flush().unwrap();
        if label == "truncated" {
            close_worker_input(&mut worker);
        }
        assert_protocol_error_closes(&mut worker, false);
        assert_protocol_session_lease_released(&root, &home, &url, &id);
        assert!(
            matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "an idle protocol-error journey contacted a provider"
        );
    }
}

#[test]
fn invalid_input_cancels_pending_approval_and_drains_completion() {
    let tmp = support::TempDir::new("native-input-pending-approval");
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    support::git_repo(&root, &[("keep.txt", "before\n")]);
    let (url, server) = provider(vec![tool("write_file", json!({"path":"keep.txt","content":"never"}))]);
    let mut worker = Worker::start(&root, &home, &url, &["--permissions", "ask"]);
    let id = worker.until("session.snapshot")["payload"]["session_id"].as_str().unwrap().to_string();
    worker.send("pending", "message.submit", json!({"text":"update keep"}));
    worker.until("approval.requested");
    worker.input.write_all(b"{broken JSON}\n").unwrap();
    worker.input.flush().unwrap();
    assert_protocol_error_closes(&mut worker, true);
    assert_eq!(std::fs::read_to_string(root.join("keep.txt")).unwrap(), "before\n");
    assert_eq!(server.join().unwrap().len(), 1, "only the initial scripted request is allowed");
    assert_protocol_session_lease_released(&root, &home, "http://127.0.0.1:9/v1", &id);
}
