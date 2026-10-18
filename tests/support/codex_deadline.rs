use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

struct StalledRefresh {
    url: String,
    returned: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Result<(bool, bool)>>>,
}

impl StalledRefresh {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let returned = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let caller_returned = returned.clone();
        // A readiness timeout must not leave Drop joining a blocked sender.
        let (ready, started) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || -> Result<(bool, bool)> {
            ready.send(())?;
            let end = std::time::Instant::now() + Duration::from_secs(60);
            let mut stream = loop {
                anyhow::ensure!(!stopping.load(Ordering::SeqCst), "refresh fixture cancelled");
                anyhow::ensure!(std::time::Instant::now() < end, "refresh accept watchdog");
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        // A request can fail before connecting. Joining must
                        // not strand the fixture in accept in that case.
                        if caller_returned.load(Ordering::SeqCst) {
                            return Ok((false, false));
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_nonblocking(true)?;
            let mut request = Vec::new();
            let mut stalled = false;
            let mut buffer = [0; 1024];
            loop {
                anyhow::ensure!(!stopping.load(Ordering::SeqCst), "refresh fixture cancelled");
                anyhow::ensure!(std::time::Instant::now() < end, "refresh EOF watchdog");
                match stream.read(&mut buffer) {
                    Ok(0) => return Ok((stalled, true)),
                    Ok(n) => {
                        request.extend_from_slice(&buffer[..n]);
                        anyhow::ensure!(request.len() <= 64 * 1024, "oversized refresh fixture request");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return Ok((stalled, true)),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    Err(error) => return Err(error.into()),
                }
                if !stalled {
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end])?;
                        anyhow::ensure!(headers.starts_with("POST "), "expected refresh POST");
                        let size = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>())
                            })
                            .ok_or_else(|| anyhow!("missing refresh content length"))??;
                        anyhow::ensure!(size <= 64 * 1024 - end - 4, "oversized refresh body");
                        if request.len() >= end + 4 + size {
                            let body: Value = serde_json::from_slice(&request[end + 4..end + 4 + size])?;
                            anyhow::ensure!(body["grant_type"] == "refresh_token", "expected OAuth refresh body");
                            // No response body or scheduled EOF can make the
                            // caller return. Only its request timeout can.
                            match stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n")
                            {
                                Ok(()) => stalled = true,
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                                    ) =>
                                {
                                    return Ok((false, true))
                                }
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                }
            }
        });
        let fixture = Self { url, returned, stop, worker: Some(worker) };
        started.recv_timeout(Duration::from_secs(60)).unwrap();
        fixture
    }

    fn finish(&mut self) -> Result<(bool, bool)> {
        self.returned.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap()
    }
}

impl Drop for StalledRefresh {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[test]
fn stalled_refresh_is_bounded_by_its_request_timeout() {
    struct Home(PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let dir = Home(auth_temp_dir());
    let path = dir.0.join("auth.json");
    std::fs::write(
        &path,
        json!({"tokens": {"access_token": jwt(json!({"exp": rusty::unix_now() + 60})), "refresh_token": "offline-r0", "account_id": "a"}})
            .to_string(),
    ).unwrap();
    let mut auth = Auth::load_from(path).unwrap();
    let mut server = StalledRefresh::start();
    let http = reqwest::blocking::Client::builder().timeout(None).build().unwrap();
    // This is an explicit fixture deadline, independent of the production
    // refresh default. Leave scheduler headroom before the real body stall.
    let result = auth.refresh_at(&http, Duration::from_secs(2), &server.url);
    let observation = server.finish();
    let error = result.expect_err("a refresh with no response body must fail");
    let timeout = error.chain().filter_map(|e| e.downcast_ref::<reqwest::Error>()).any(reqwest::Error::is_timeout);
    assert!(timeout, "refresh must fail from its request deadline, not fixture EOF: {error:#}");
    let (stalled, closed) = observation.unwrap();
    assert!(stalled, "refresh must send a complete OAuth POST and receive partial response headers");
    assert_eq!(error.to_string(), "reading codex token refresh response", "deadline must expire in the stalled body");
    assert!(closed, "request timeout must close the peer transport before the fixture releases it");
}

#[test]
fn refresh_fixture_stops_if_the_request_never_connects() {
    let mut server = StalledRefresh::start();
    assert_eq!(server.finish().unwrap(), (false, false));
    assert_eq!(Arc::strong_count(&server.stop), 1, "joined worker retained fixture state");
}
