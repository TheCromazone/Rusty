//! Scheduler tolerance is a deadlock guard, never evidence that an event ran.
use std::time::{Duration, Instant};
pub fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ready() {
        assert!(start.elapsed() < Duration::from_secs(60), "waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Consume the complete HTTP request before a silent-response fixture stalls.
/// A single TCP read is not a request boundary.
pub fn request(stream: &mut std::net::TcpStream) {
    use std::io::{BufRead, BufReader, Read};
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut reader = BufReader::new(stream);
    let mut size = 0;
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0, "EOF in HTTP headers");
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            if key.eq_ignore_ascii_case("content-length") {
                size = value.trim().parse().unwrap();
            }
        }
    }
    reader.read_exact(&mut vec![0; size]).unwrap();
}

/// Drain both pipes while the process runs. Waiting for exit before reading
/// can deadlock once either pipe fills, especially with a resumed transcript.
pub fn wait(child: std::process::Child, limit: Duration) -> std::process::Output {
    use std::os::fd::AsRawFd;
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn drain<R: std::io::Read>(pipe: &mut Option<R>, bytes: &mut Vec<u8>) -> bool {
        let Some(pipe) = pipe else { return true };
        let mut buf = [0; 8192];
        for _ in 0..32 {
            match pipe.read(&mut buf) {
                Ok(0) => return true,
                Ok(n) => {
                    bytes.extend_from_slice(&buf[..n]);
                    assert!(bytes.len() <= 16 << 20, "fixture output exceeded 16 MiB");
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("reading fixture pipe: {e}"),
            }
        }
        false
    }
    let mut child = Reap(child);
    let (mut stdout, mut stderr) = (child.0.stdout.take(), child.0.stderr.take());
    for fd in stdout.iter().map(AsRawFd::as_raw_fd).chain(stderr.iter().map(AsRawFd::as_raw_fd)) {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }, -1);
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let start = Instant::now();
    loop {
        drain(&mut stdout, &mut out);
        drain(&mut stderr, &mut err);
        if let Some(status) = child.0.try_wait().unwrap() {
            // All writes by this process precede its exit. Close our readers
            // after buffered data, even if a descendant retained a writer.
            if drain(&mut stdout, &mut out) && drain(&mut stderr, &mut err) {
                return std::process::Output { status, stdout: out, stderr: err };
            }
        }
        assert!(
            start.elapsed() <= limit,
            "fixture did not exit within {limit:?}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&err)
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn waiting_drains_more_than_a_pipeful_from_both_streams() {
        let child = std::process::Command::new("python3")
            .args(["-c", "import sys; sys.stdout.write('o'*1048576); sys.stderr.write('e'*1048576)"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let out = super::wait(child, std::time::Duration::from_secs(120));
        assert!(out.status.success());
        assert_eq!(out.stdout, vec![b'o'; 1048576]);
        assert_eq!(out.stderr, vec![b'e'; 1048576]);
    }
}
