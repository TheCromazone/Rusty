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
