use super::*;
use crate::budget::{Budget, Limits};
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
        elapsed.store(600_000, Ordering::SeqCst);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")?;
        stream.flush()?;
        released.recv_timeout(Duration::from_secs(60))?;
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
        // The fixture owns this group, including on an assertion failure.
        if self.1 {
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        }
        let _ = self.0.wait();
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
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = FixtureDir(std::env::temp_dir().join(format!(
        "rusty-deadline-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir(&root.0).unwrap();
    let clock = root.0.join("clock");
    std::fs::write(&clock, "0").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let output = std::fs::File::create(root.0.join("child.log")).unwrap();
    let mut child = ChildFixture(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent::budget_deadline_tests::deadline_child", "--nocapture", "--test-threads", "1"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root.0)
            .env("RUSTY_HOME", &root.0)
            .env("RUSTY_INFRA", "off")
            .env("RUSTY_NO_DOTENV", "1")
            .env("RUSTY_STREAM_IDLE_SECS", "300")
            .env("RUSTY_UNIT_DEADLINE_CLOCK", &clock)
            .env("RUSTY_UNIT_DEADLINE_URL", url)
            .current_dir(&root.0)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
        true,
    );
    let server = std::thread::spawn(move || -> Result<()> {
        let end = Instant::now() + Duration::from_secs(60);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < end => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e.into()),
            }
        };
        read_model_post(&mut stream)?;
        let next = clock.with_extension("next");
        std::fs::write(&next, "600000")?;
        std::fs::rename(next, clock)?;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n")?;
        stream.flush()?;
        let mut byte = [0];
        if stream.read(&mut byte)? != 0 {
            bail!("deadline exit must close the silent response");
        }
        Ok(())
    });
    let end = Instant::now() + Duration::from_secs(120);
    let outcome = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) => break Err("deadline child did not exit".to_string()),
            Err(e) => break Err(e.to_string()),
        }
    };
    if outcome.is_err() {
        unsafe { libc::kill(-(child.0.id() as i32), libc::SIGKILL) };
        let _ = child.0.wait();
    }
    // Join before asserting so every failure path closes the mock as well.
    let server_result = server.join();
    let group_absent = unsafe { libc::kill(-(child.0.id() as i32), 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    child.1 = !group_absent;
    let mut log = String::new();
    std::fs::File::open(root.0.join("child.log")).unwrap().take(64 * 1024).read_to_string(&mut log).unwrap();
    assert!(outcome.unwrap().success(), "{log}");
    assert!(group_absent, "deadline child group must be gone before fixture cleanup");
    server_result.unwrap().unwrap();
    let trace: Value = serde_json::from_slice(&std::fs::read(root.0.join("outcome.json")).unwrap()).unwrap();
    assert_eq!(trace["requests"], 1);
    assert_eq!(trace["unknown_usage_requests"], 1);
    assert_eq!(trace["deadline_reached"], true);
}

// Executed in isolation by the transport test. In an ordinary suite this is
// inert. Its file-clock configuration exists only in this cfg(test) module.
#[test]
fn deadline_child() {
    let Some(clock) = std::env::var_os("RUSTY_UNIT_DEADLINE_CLOCK") else { return };
    let clock = PathBuf::from(clock);
    let outcome = clock.with_file_name("outcome.json");
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
