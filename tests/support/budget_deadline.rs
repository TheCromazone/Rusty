use super::*;
use crate::budget::{Budget, Limits};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
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
