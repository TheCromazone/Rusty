//! Offline cloud preparation benchmark: 64 MiB context, loopback object store.
//! Run under /usr/bin/time or wait4 to compare process CPU and peak RSS.
use anyhow::{Context as _, Result};
use rusty::cloud::{
    api::PushAccess,
    digest::Sha256,
    image::{self, Context},
};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant, UNIX_EPOCH};

fn main() -> Result<()> {
    let path = std::env::temp_dir().join(format!("rusty-cloud-perf-{}", std::process::id()));
    let result = run(&path);
    let _ = std::fs::remove_file(path);
    result
}

fn run(path: &std::path::Path) -> Result<()> {
    let mut f = std::fs::File::create(path)?;
    let block: Vec<u8> = (0..65536).map(|i| (i % 251) as u8).collect();
    for _ in 0..1024 {
        f.write_all(&block)?;
    }
    f.set_modified(UNIX_EPOCH + Duration::from_secs(1))?;
    drop(f);
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let storage_url = format!("http://{}", listener.local_addr()?);
    let server = std::thread::spawn(move || -> Result<(u64, String)> {
        let mut receipt = (0, String::new());
        for _ in 0..2 {
            let (socket, _) = listener.accept()?;
            socket.set_read_timeout(Some(Duration::from_secs(30)))?;
            let mut reader = BufReader::new(socket);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let head = line.starts_with("HEAD ");
            let mut length = 0u64;
            let mut expected = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                if line == "\r\n" {
                    break;
                }
                if let Some((key, value)) = line.split_once(':') {
                    match key.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse()?,
                        "x-amz-content-sha256" => expected = value.trim().to_string(),
                        _ => {}
                    }
                }
            }
            if !head {
                let mut h = Sha256::default();
                let mut bytes = [0u8; 65536];
                let mut left = length;
                while left > 0 {
                    let take = (left as usize).min(bytes.len());
                    let n = reader.read(&mut bytes[..take])?;
                    anyhow::ensure!(n > 0, "short upload");
                    h.update(&bytes[..n]);
                    left -= n as u64;
                }
                let hash = h.hex();
                anyhow::ensure!(hash == expected, "signature payload changed");
                receipt = (length, hash);
            }
            write!(
                reader.get_mut(),
                "HTTP/1.1 {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                if head { "404 Not Found" } else { "200 OK" }
            )?;
        }
        Ok(receipt)
    });
    let access = PushAccess {
        access_key: "offline".into(),
        secret: "offline".into(),
        session_token: "offline".into(),
        storage_url,
        organization_id: "offline".into(),
        bucket: "build".into(),
        region: "local".into(),
    };
    let start = Instant::now();
    let hashes = image::upload_contexts(&access, &[Context { source: path.into(), archive: "fixture".into() }])?;
    let (bytes, sha) = server.join().ok().context("object store panicked")??;
    println!(
        "{}",
        serde_json::json!({"wall_ms":start.elapsed().as_secs_f64()*1000.0,"context_hash":hashes[0],"tar_bytes":bytes,"tar_sha256":sha})
    );
    Ok(())
}
