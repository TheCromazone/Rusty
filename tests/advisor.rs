//! Real-process service tests. No model or cloud account is involved.
use rusty::advisor::{rpc, Level, Request, Response, KNOWN_PREFIX, SAVED_PREFIX};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
static NEXT: AtomicU64 = AtomicU64::new(1);

struct Service {
    root: PathBuf,
    child: Child,
}
impl Service {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "rm-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd"))
            .arg("--home")
            .arg(&root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let s = Self { root, child };
        s.ready();
        s
    }
    fn ready(&self) {
        let start = Instant::now();
        while rpc(
            &self.root.join("memory/advisor.sock"),
            &Request {
                op: "status".into(),
                scope: "ready".into(),
                session: "ready".into(),
                seq: 0,
                level: Level::Learn,
                data: Value::Null,
            },
        )
        .is_err()
        {
            assert!(start.elapsed() < Duration::from_secs(60));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn call(&self, op: &str, scope: &str, seq: u64, data: Value) -> Response {
        use std::io::{BufRead, BufReader, Write};
        // These cases exercise the database, not the production RPC deadline.
        // The dedicated transport test below still uses the real rpc client.
        let mut stream = std::os::unix::net::UnixStream::connect(self.root.join("memory/advisor.sock")).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(60))).unwrap();
        let request =
            Request { op: op.into(), scope: scope.into(), session: "session".into(), seq, level: Level::Learn, data };
        writeln!(stream, "{}", serde_json::to_string(&request).unwrap()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn remembering_a_near_duplicate_reports_known_without_adding_a_row() {
    let s = Service::new();
    let first =
        s.call("remember", "repo-a", 1, json!({"kind":"fact","text":"Always inspect workspace packages manifest"}));
    assert!(!first.error, "first remember failed: {}", first.text);
    assert!(first.text.starts_with(SAVED_PREFIX), "unexpected first reply: {}", first.text);
    assert!(!first.id.is_empty());

    let second =
        s.call("remember", "repo-a", 2, json!({"kind":"fact","text":"Always inspect workspace packages manifest"}));
    assert!(!second.error, "duplicate remember failed: {}", second.text);
    assert!(second.text.starts_with(KNOWN_PREFIX), "unexpected duplicate reply: {}", second.text);
    assert_eq!(second.id, first.id, "duplicate should retain the existing lesson id");
    assert_eq!(s.call("status", "repo-a", 2, Value::Null).data["lessons"], 1);
}

#[test]
fn remembering_a_command_extension_or_opposite_polarity_keeps_both_facts() {
    let s = Service::new();
    for (seq, text) in [
        (1, "run cargo test"),
        (2, "run cargo test --release"),
        (3, "deploy production"),
        (4, "never deploy production"),
        (5, "do not deploy production"),
    ] {
        let saved = s.call("remember", "repo-a", seq, json!({"kind":"fact","text":text}));
        assert!(!saved.error, "remember failed for {text}: {}", saved.text);
        assert!(saved.text.starts_with(SAVED_PREFIX), "{text} was collapsed: {}", saved.text);
    }
    assert_eq!(s.call("status", "repo-a", 5, Value::Null).data["lessons"], 5);
}

#[test]
fn daemon_restarts_preserve_l2_but_not_temporary_observations() {
    let mut s = Service::new();
    for _ in 0..30 {
        drop(std::os::unix::net::UnixStream::connect(s.root.join("memory/advisor.sock")).unwrap());
        assert!(!s.call("status", "ready", 0, Value::Null).error);
    }
    let id =
        s.call("remember", "repo-a", 0, json!({"kind":"fact","text":"Always inspect workspace packages manifest"})).id;
    assert!(!id.is_empty());
    assert!(!s.call("begin", "repo-a", 1, json!({"text":"inspect workspace packages manifest"})).error);
    assert_eq!(s.call("advice", "repo-a", 1, Value::Null).id, id);
    assert!(s.call("advice", "repo-a", 1, Value::Null).text.is_empty(), "duplicate advice");
    assert!(!s.call("feedback", "repo-a", 1, json!({"helpful":true})).error);
    let snapshot = s.call("export", "repo-a", 1, Value::Null).data;
    assert_eq!(s.call("list", "repo-b", 0, Value::Null).text, "no memories saved yet");
    assert!(s.call("event", "repo-a", 1, json!({})).error, "stale observation accepted");
    let second = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd")).arg("--home").arg(&s.root).output().unwrap();
    assert!(!second.status.success());
    assert_eq!(std::fs::metadata(s.root.join("memory")).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(s.root.join("memory/advisor.sock")).unwrap().permissions().mode() & 0o777, 0o600);
    s.child.kill().unwrap();
    s.child.wait().unwrap();
    s.child = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd")).arg("--home").arg(&s.root).spawn().unwrap();
    let start = Instant::now();
    loop {
        if rpc(
            &s.root.join("memory/advisor.sock"),
            &Request {
                op: "status".into(),
                scope: "repo-a".into(),
                session: "session".into(),
                seq: 1,
                level: Level::Learn,
                data: Value::Null,
            },
        )
        .is_ok()
        {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = s.call("status", "repo-a", 1, Value::Null).data;
    // `/memory helpful` counts as two verified outcomes and survives a restart.
    assert_eq!(status["outcomes"], 2);
    assert!(status["l1"].is_null(), "short-term observations survived restart");
    s.call("forget", "repo-a", 1, json!({"id":id}));
    assert!(!s.call("import", "repo-a", 1, snapshot).error);
    assert_eq!(s.call("list", "repo-a", 1, Value::Null).text, "no memories saved yet", "deleted lesson resurrected");
}

#[test]
fn an_empty_recall_says_whether_anything_is_saved() {
    let s = Service::new();
    assert_eq!(s.call("recall", "repo-a", 0, json!({"query":"packages"})).text, "no memories saved yet");
    let id = s.call("remember", "repo-a", 0, json!({"kind":"fact","text":"Run the workspace tests with cargo"})).id;
    assert!(!id.is_empty());
    // A query that matches nothing still says the memory is not empty.
    assert_eq!(
        s.call("recall", "repo-a", 0, json!({"query":"kubernetes"})).text,
        "no memories match; 1 saved in total"
    );
    assert!(s.call("recall", "repo-a", 0, json!({"query":"workspace tests"})).text.contains(&id));
    assert!(s.call("list", "repo-a", 0, Value::Null).text.contains(&id));
}

#[test]
fn export_larger_than_hook_frame_roundtrips_and_invalid_import_rolls_back() {
    let s = Service::new();
    // Distinct words, so near-duplicate merging keeps every lesson.
    for i in 0..60 {
        let text: Vec<String> = (0..110).map(|j| format!("w{i}x{j}")).collect();
        assert!(!s.call("remember", "a", 0, json!({"text":text.join(" ")})).error);
    }
    let mut snapshot = s.call("export", "a", 0, Value::Null).data;
    assert!(serde_json::to_vec(&snapshot).unwrap().len() > 32768);
    assert!(!s.call("import", "a", 0, snapshot.clone()).error);
    snapshot["lessons"][0]["id"] = json!("0000000000000000");
    assert!(s.call("import", "a", 0, snapshot).error);
    assert_eq!(s.call("status", "a", 0, Value::Null).data["lessons"], 60);
}

#[test]
fn direct_ingress_redacts_secrets_and_l1_never_reaches_disk() {
    let s = Service::new();
    let saved = s.call(
        "remember",
        "privacy",
        0,
        json!({"text":"DB_PASSWORD=secret-canary-12345 Authorization: Bearer evidence-canary-12345"}),
    );
    assert!(!saved.error);
    let exported = s.call("export", "privacy", 0, Value::Null).data.to_string();
    assert!(exported.contains("[redacted]"));
    assert!(!exported.contains("secret-canary") && !exported.contains("evidence-canary"));
    s.call("begin", "privacy", 1, json!({"text":"temporary-request-canary"}));
    s.call(
        "event",
        "privacy",
        2,
        json!({"tool":"bash","args":"temporary-args-canary","output":"temporary-output-canary"}),
    );
    let db = rusqlite::Connection::open(s.root.join("memory/memory.sqlite")).unwrap();
    let traces: i64 =
        db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('boards','events')", [], |r| r.get(0)).unwrap();
    assert_eq!(traces, 0);
    drop(db);
    for entry in std::fs::read_dir(s.root.join("memory")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let bytes = std::fs::read(path).unwrap();
            for canary in
                ["secret-canary-12345", "temporary-request-canary", "temporary-args-canary", "temporary-output-canary"]
            {
                assert!(!bytes.windows(canary.len()).any(|w| w == canary.as_bytes()));
            }
        }
    }
    assert!(!s.call("finish", "privacy", 3, json!({"complete":true})).error);
    assert_eq!(s.call("status", "privacy", 3, Value::Null).data["l1"]["observations"], 0);
}

#[test]
fn daemon_refuses_symlinked_persistence() {
    use std::os::unix::fs::symlink;
    let s = Service::new();
    let root = s.root.join("symlink-test");
    std::fs::create_dir(&root).unwrap();
    symlink(s.root.join("memory"), root.join("memory")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd")).arg("--home").arg(root).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn terminal_interrupt_does_not_kill_autostarted_memory() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::process::CommandExt;
    let root =
        PathBuf::from("/tmp").join(format!("rm-auto-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&root).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_rusty"))
        .args(["--memory", "learn", "--mode", "vibe"])
        .current_dir(&root)
        .env("RUSTY_HOME", &root)
        .env("HOME", &root)
        .env("RUSTY_NO_DOTENV", "1")
        .env("RUSTY_INFRA", "off")
        .env("NVIDIA_API_KEY", "offline-test-key")
        .env_remove("RUSTY_API_KEY")
        .env("PATH", "/usr/bin:/bin")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut s = Service { root, child };
    s.ready();
    let stdout = s.child.stdout.take().unwrap();
    let (ready, observed) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.contains("/goal") {
                let _ = ready.send(());
            }
        }
    });
    writeln!(s.child.stdin.as_mut().unwrap(), "/help").unwrap();
    observed.recv_timeout(Duration::from_secs(60)).expect("REPL help before SIGINT");
    assert!(!s.call("status", "test", 0, Value::Null).error);
    unsafe {
        assert_eq!(libc::kill(-(s.child.id() as i32), libc::SIGINT), 0);
    }
    writeln!(s.child.stdin.take().unwrap(), "/exit").unwrap();
    s.child.wait().unwrap();
    reader.join().unwrap();
    assert!(!s.call("status", "test", 0, Value::Null).error, "Ctrl-C killed the memory service");
    let stopped =
        Command::new(env!("CARGO_BIN_EXE_rusty-memoryd")).arg("--home").arg(&s.root).arg("stop").output().unwrap();
    assert!(stopped.status.success(), "{}", String::from_utf8_lossy(&stopped.stderr));
}

#[test]
fn failed_database_initialization_never_publishes_ready_socket() {
    let root = PathBuf::from("/tmp").join(format!(
        "rm-invalid-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(root.join("memory")).unwrap();
    std::fs::write(root.join("memory/memory.sqlite"), b"invalid SQLite fixture").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd")).arg("--home").arg(&root).output().unwrap();
    assert!(!result.status.success());
    assert!(!root.join("memory/advisor.sock").exists(), "failed initialization published a readiness socket");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn daemon_does_not_retain_unrelated_inherited_pipe_writers() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    for fd in fds {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
    }
    let root =
        PathBuf::from("/tmp").join(format!("rm-fd-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&root).unwrap();
    let fd = writer.as_raw_fd();
    let mut command = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd"));
    command.arg("--home").arg(&root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            // Model an unrelated launcher pipe without close-on-exec.
            if libc::fcntl(fd, libc::F_DUPFD, 200) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let service = Service { root, child: command.spawn().unwrap() };
    drop(writer);
    service.ready();
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0);
    let mut byte = [0];
    assert_eq!(reader.read(&mut byte).unwrap(), 0, "daemon retained a foreign writer after startup");
    assert!(!service.call("status", "fd-test", 0, Value::Null).error);
}
