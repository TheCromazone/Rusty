use super::*;
use crate::budget::{Budget, Limits};
use anyhow::Context;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;

struct ExpireOnExit(Arc<AtomicU64>);
impl Drop for ExpireOnExit {
    fn drop(&mut self) {
        // A fixture error must release the agent too; it is reported after
        // joining instead of stranding the test in a provider retry.
        self.0.store(600_000, Ordering::SeqCst);
    }
}

fn read_model_post(stream: &mut TcpStream) -> Result<()> {
    // Accepted sockets may inherit O_NONBLOCK; timeouts do not clear it.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut size = None;
    let mut headers = String::new();
    let mut complete = false;
    // Bound fixture input as well as its lifetime.
    while headers.len() <= 16 * 1024 {
        let mut line = String::new();
        if reader.by_ref().take(16 * 1024).read_line(&mut line)? == 0 {
            bail!("EOF before complete request headers");
        }
        if line == "\r\n" {
            complete = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                size = Some(value.trim().parse::<usize>()?);
            }
        }
        headers.push_str(&line);
    }
    if !complete {
        bail!("request headers exceed fixture limit");
    }
    anyhow::ensure!(headers.starts_with("POST "), "expected model POST, got {:?}", headers.lines().next());
    let size = size.filter(|size| *size <= 64 * 1024).ok_or_else(|| anyhow::anyhow!("unbounded request"))?;
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body)?;
    if !headers.starts_with("POST ") || request["messages"].as_array().is_none() {
        bail!("expected a complete model POST");
    }
    Ok(())
}

#[test]
fn request_reader_uses_blocking_io() {
    use std::os::fd::AsRawFd;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut stream, _) = listener.accept().unwrap();
    stream.set_nonblocking(true).unwrap();
    let witness = stream.try_clone().unwrap();
    let body = r#"{"messages":[]}"#;
    let request = format!("POST /v1/chat/completions HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    client.write_all(request.as_bytes()).unwrap();
    read_model_post(&mut stream).expect("request reader must clear inherited nonblocking before bounded reads");
    let flags = unsafe { libc::fcntl(witness.as_raw_fd(), libc::F_GETFL) };
    assert_ne!(flags, -1);
    assert_eq!(flags & libc::O_NONBLOCK, 0, "request reader must clear inherited nonblocking");
}

#[test]
fn root_budget_deadline_stops_a_silent_response() {
    let elapsed = Arc::new(AtomicU64::new(0));
    let clock = elapsed.clone();
    // Keep the real request timer beyond the fixture watchdog. Otherwise a
    // missing agent poll could accidentally pass when the real timer fires.
    let budget = Budget::new_with_clock(
        Limits { seconds: 600, ..Limits::default() },
        Arc::new(move || Duration::from_millis(clock.load(Ordering::SeqCst))),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = crate::llm::Endpoint::new(
        crate::config::Provider::Compatible,
        format!("http://{}/v1", listener.local_addr().unwrap()),
        vec!["offline-test-key".into()],
    );
    let mut client = Client::new(vec![endpoint]).unwrap();
    client.budget = budget.clone();
    let client = Arc::new(client);
    let (release, released) = mpsc::channel();
    let server = std::thread::spawn(move || -> Result<()> {
        let _expire = ExpireOnExit(elapsed.clone());
        let end = Instant::now() + Duration::from_secs(60);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if released.try_recv() != Err(mpsc::TryRecvError::Empty) || Instant::now() >= end {
                        bail!("agent returned without sending a request");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        };
        read_model_post(&mut stream)?;
        // Only a complete real request advances provider time. Keep the
        // socket open until ask_model has returned, so EOF cannot pass it.
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")?;
        stream.flush()?;
        elapsed.store(600_000, Ordering::SeqCst);
        released
            .recv_timeout(Duration::from_secs(60))
            .context("silent response was not released before fixture watchdog")?;
        Ok(())
    });
    // Keep unrelated signal tests from changing this turn's cancellation.
    let _cancel = signal::cancel_scope(Arc::new(AtomicBool::new(false)));
    let mut agent = Agent::new(client.clone(), "test-model".into(), std::env::temp_dir(), Policy::new(Mode::Yolo));
    let result = agent.ask_model(
        "test-model".into(),
        &mut Display::new(false),
        Arc::new(vec![json!({"role":"user", "content":"inspect"})]),
        json!([]),
        16,
    );
    let _ = release.send(());
    let server_result = server.join();
    drop(agent);
    // Closing the fixture releases the blocking reader. Observe the
    // producer's final Arc drop instead of leaving a detached worker alive.
    let end = Instant::now() + Duration::from_secs(60);
    while Arc::strong_count(&client) != 1 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(Arc::strong_count(&client), 1, "provider worker must release the client");
    server_result.unwrap().unwrap();
    let error = match result {
        Err(error) => error.to_string(),
        Ok(_) => panic!("silent request must stop at the root deadline"),
    };
    assert!(error.contains("provider deadline reached"), "{error}");
    let snapshot = serde_json::to_value(budget.snapshot()).unwrap();
    assert_eq!(snapshot["requests"], 1);
    assert_eq!(snapshot["unknown_usage_requests"], 1);
    assert_eq!(snapshot["active_requests"], 0);
    assert_eq!(snapshot["deadline_reached"], true);
}

struct ChildFixture(std::process::Child, bool);
impl Drop for ChildFixture {
    fn drop(&mut self) {
        // Keep the direct child in the harness group so outer cancellation
        // also reaches it. This handle owns its PID only until wait/reap.
        if self.1 {
            let _ = self.0.kill();
            self.1 = false;
            let _ = self.0.wait();
        }
    }
}

struct FixtureDir(PathBuf);
impl Drop for FixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn deadline_exit_closes_the_silent_transport() {
    use std::process::{Command, Stdio};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root_path = std::env::temp_dir().join(format!(
        "rusty-deadline-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // Own cleanup only after exclusive creation succeeds.
    std::fs::create_dir(&root_path).unwrap();
    let root = FixtureDir(root_path);
    let clock = root.0.join("clock");
    std::fs::write(&clock, "0").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let output = std::fs::File::create(root.0.join("child.log")).unwrap();
    let mut child = ChildFixture(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::budget_deadline_tests::deadline_child",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root.0)
            .env("RUSTY_HOME", &root.0)
            .env("RUSTY_INFRA", "off")
            .env("RUSTY_NO_DOTENV", "1")
            .env("RUSTY_STREAM_IDLE_SECS", "300")
            .env("RUSTY_RETRY_SECS", "1")
            .env("RUSTY_UNIT_DEADLINE_CLOCK", &clock)
            .env("RUSTY_UNIT_DEADLINE_URL", url)
            .current_dir(&root.0)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
        true,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let (completed, completion) = mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let end = Instant::now() + Duration::from_secs(60);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < end
                            && !stopping.load(Ordering::SeqCst) =>
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(e) => return Err(e.into()),
                }
            };
            read_model_post(&mut stream)?;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")?;
            stream.flush()?;
            let next = clock.with_extension("next");
            std::fs::write(&next, "600000")?;
            std::fs::rename(next, clock)?;
            let mut byte = [0];
            if stream
                .read(&mut byte)
                .context("deadline child did not close the silent response before fixture watchdog")?
                != 0
            {
                bail!("deadline exit must close the silent response");
            }
            Ok(())
        })();
        let _ = completed.send(result.is_err());
        result
    });
    let end = Instant::now() + Duration::from_secs(120);
    let outcome = loop {
        if completion.try_recv() == Ok(true) {
            break Err("silent transport fixture failed".to_string());
        }
        match child.0.try_wait() {
            Ok(Some(status)) => {
                child.1 = false; // try_wait reaped: never signal this PID again.
                break Ok(status);
            }
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) => break Err("deadline child did not exit".to_string()),
            Err(e) => {
                child.1 = false; // Ownership uncertain; do not signal an identifier.
                break Err(e.to_string());
            }
        }
    };
    if !outcome.as_ref().is_ok_and(|status| status.success()) {
        stop.store(true, Ordering::SeqCst);
    }
    if outcome.is_err() && child.1 {
        let _ = child.0.kill();
        child.1 = false;
        let _ = child.0.wait();
    }
    // Join before asserting so every failure path closes the mock as well.
    let server_result = server.join();
    let mut log = String::new();
    std::fs::File::open(root.0.join("child.log")).unwrap().take(64 * 1024).read_to_string(&mut log).unwrap();
    let server_result =
        server_result.map_err(|_| "fixture thread panicked".to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
    assert!(outcome.as_ref().is_ok_and(|status| status.success()), "{outcome:?}; fixture: {server_result:?}\n{log}");
    server_result.unwrap();
    let process: Value = serde_json::from_slice(&std::fs::read(root.0.join("process.json")).unwrap()).unwrap();
    assert_eq!(process["pid"].as_u64(), Some(child.0.id() as u64));
    assert_eq!(
        process["pgid"].as_i64(),
        Some(unsafe { libc::getpgrp() } as i64),
        "child must remain contained by the outer harness group"
    );
    let trace: Value = serde_json::from_slice(&std::fs::read(root.0.join("outcome.json")).unwrap()).unwrap();
    assert_eq!(trace["requests"], 1);
    assert_eq!(trace["unknown_usage_requests"], 1);
    assert_eq!(trace["deadline_reached"], true);
}

// The parent transport test invokes this helper as its only ignored test.
#[test]
#[ignore = "subprocess helper invoked by deadline_exit_closes_the_silent_transport"]
fn deadline_child() {
    let clock =
        PathBuf::from(std::env::var_os("RUSTY_UNIT_DEADLINE_CLOCK").expect("transport parent supplies the clock"));
    let outcome = clock.with_file_name("outcome.json");
    let process = json!({"pid":std::process::id(), "pgid":unsafe { libc::getpgrp() }});
    std::fs::write(clock.with_file_name("process.json"), serde_json::to_vec(&process).unwrap()).unwrap();
    let budget = Budget::new_with_clock(
        Limits { seconds: 600, ..Limits::default() },
        Arc::new(move || Duration::from_millis(std::fs::read_to_string(&clock).unwrap().parse().unwrap())),
    );
    let endpoint = crate::llm::Endpoint::new(
        crate::config::Provider::Compatible,
        std::env::var("RUSTY_UNIT_DEADLINE_URL").unwrap(),
        vec!["offline-test-key".into()],
    );
    let mut client = Client::new(vec![endpoint]).unwrap();
    client.budget = budget.clone();
    let _cancel = signal::cancel_scope(Arc::new(AtomicBool::new(false)));
    let mut agent =
        Agent::new(Arc::new(client), "test-model".into(), std::env::current_dir().unwrap(), Policy::new(Mode::Yolo));
    let result = agent.ask_model(
        "test-model".into(),
        &mut Display::new(false),
        Arc::new(vec![json!({"role":"user", "content":"inspect"})]),
        json!([]),
        16,
    );
    let error = match result {
        Err(error) => error.to_string(),
        Ok(_) => panic!("silent request must stop at the root deadline"),
    };
    assert!(error.contains("provider deadline reached"), "{error}");
    std::fs::write(outcome, serde_json::to_vec(&budget.snapshot()).unwrap()).unwrap();
    // Returning lets this one-test process exit. The parent independently
    // observes EOF; it does not release a server socket to make us finish.
}
