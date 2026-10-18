use super::*;

#[test]
fn management_transport_uses_one_second_and_hooks_use_two_hundred_ms() {
    for (op, timeout) in [("status", 1000), ("advice", 200)] {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let witness = stream.try_clone().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(60))).unwrap();
        // Queue the response before entering RPC: no other thread must run
        // during the short transport window. Socket options are shared by dup.
        let response = Response { text: "ready".into(), ..Response::default() };
        writeln!(peer, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        let request = Request {
            op: op.into(),
            scope: "test".into(),
            session: "test".into(),
            seq: 0,
            level: Level::Learn,
            data: Value::Null,
        };
        let response = rpc_on_stream(stream, &request).unwrap();
        assert_eq!(response.text, "ready");
        let expected = Some(Duration::from_millis(timeout));
        assert_eq!(witness.read_timeout().unwrap(), expected, "{op} read deadline");
        assert_eq!(witness.write_timeout().unwrap(), expected, "{op} write deadline");
        let mut line = String::new();
        BufReader::new(peer).read_line(&mut line).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap().op, op);
    }
}

fn queued_hooks() -> (Hooks, mpsc::Receiver<Work>) {
    let (tx, pending) = mpsc::sync_channel(8);
    let hooks = Hooks {
        tx,
        wait: None,
        worker_life: std::sync::Weak::new(),
        level: Level::Learn,
        scope: "test".into(),
        session: "test".into(),
        seq: 0,
        cwd: PathBuf::new(),
        root: PathBuf::new(),
        delivered: Vec::new(),
        preferences: String::new(),
        turn_advice: Vec::new(),
        metrics: Metrics::default(),
    };
    (hooks, pending)
}

#[test]
fn stalled_service_does_not_hold_coding_hook() {
    let (mut hooks, pending) = queued_hooks();
    hooks.wait = Some(|response, timeout| {
        assert_eq!(timeout, Duration::from_millis(50));
        assert!(matches!(response.try_recv(), Err(mpsc::TryRecvError::Empty)));
        Err(mpsc::RecvTimeoutError::Timeout)
    });
    assert!(hooks.advice(0).is_empty());
    assert_eq!(hooks.metrics.hooks, 1);
    assert_eq!(hooks.metrics.timeouts, 1);
    let (request, response) = pending.try_recv().unwrap();
    assert_eq!(request.op, "advice");
    assert!(response.send(Ok(Response::default())).is_err(), "timed-out waiter must be dropped");
}

#[test]
fn real_timed_wait_drops_the_response_receiver() {
    let (mut hooks, pending) = queued_hooks();
    // None selects the actual std recv_timeout, with no injected completion.
    assert!(hooks.advice(0).is_empty());
    assert_eq!(hooks.metrics.timeouts, 1);
    let (_, reply) = pending.try_recv().unwrap();
    assert!(reply.send(Ok(Response::default())).is_err(), "real timed-out waiter retained its receiver");
}

struct Home(PathBuf);
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct StalledSocket {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<()>>>,
}
impl Drop for StalledSocket {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn real_stalled_socket_times_out_and_joins_the_hook_worker() {
    use std::sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let home = Home(PathBuf::from("/tmp").join(format!(
        "rm-hook-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )));
    std::fs::create_dir_all(home.0.join("memory")).unwrap();
    let listener = UnixListener::bind(home.0.join("memory/advisor.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = std::thread::spawn(move || -> Result<()> {
        let end = Instant::now() + Duration::from_secs(60);
        loop {
            if stopping.load(Ordering::SeqCst) {
                bail!("stalled socket fixture cancelled");
            }
            anyhow::ensure!(Instant::now() < end, "stalled socket fixture watchdog");
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            stream.set_nonblocking(true)?;
            let mut request = Vec::new();
            let mut seen = false;
            let mut buffer = [0; 1024];
            loop {
                if stopping.load(Ordering::SeqCst) {
                    bail!("stalled socket fixture cancelled");
                }
                anyhow::ensure!(Instant::now() < end, "stalled socket request watchdog");
                match stream.read(&mut buffer) {
                    Ok(0) if seen => return Ok(()),
                    Ok(0) => break, // Hooks::connect's readiness probe.
                    Ok(n) => {
                        request.extend_from_slice(&buffer[..n]);
                        anyhow::ensure!(request.len() <= FRAME, "oversized hook fixture request");
                        if request.ends_with(b"\n") && !seen {
                            let parsed: Request = serde_json::from_slice(&request)?;
                            anyhow::ensure!(parsed.op == "advice", "expected advice hook");
                            seen = true;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1))
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    });
    let mut server = StalledSocket { stop, thread: Some(thread) };
    let mut hooks = Hooks::connect(&home.0, &home.0, Level::Learn).unwrap();
    let worker = hooks.worker_life.clone();
    let initially_live = worker.upgrade().is_some();
    let real_wait = hooks.wait.is_none();
    let advice = hooks.advice(0);
    let timeouts = hooks.metrics.timeouts;
    drop(hooks);
    let server_result = server.thread.take().unwrap().join();
    let end = Instant::now() + Duration::from_secs(60);
    while worker.upgrade().is_some() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(real_wait, "socket journey must use the real timed wait");
    assert!(initially_live, "worker lifetime witness was never live");
    assert!(worker.upgrade().is_none(), "hook worker retained after dropping Hooks");
    server_result.unwrap().unwrap();
    assert!(advice.is_empty());
    assert_eq!(timeouts, 1);
}
