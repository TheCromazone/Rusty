//! A session-scoped, authenticated localhost bridge to a sandbox's tools.
//!
//! rusty (`--tools daytona`) posts tool requests here; each one runs as
//! `rusty --tool-rpc` inside the sandbox, with the JSON passed as quoted data
//! on stdin. The Daytona connection stays in this process; no model keys,
//! dotenv files or host environment are sent to the sandbox.

use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::digest::{constant_time_eq, random_hex};
use super::shell::quote;
use super::Remote;

pub const MAX_RPC: usize = 1024 * 1024;
/// The lead plus eight workers.
const SLOTS: usize = 9;
const MAX_HEADER: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSPORT_FAILED: &str = "Remote tool transport failed; remote outcome may be unknown. \
    Inspect the remote workspace before retrying; no local fallback.";

#[derive(Debug)]
struct EndpointExit(i32);
impl std::fmt::Display for EndpointExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            127 => f.write_str("remote tool endpoint exited 127: install Rusty on the remote PATH"),
            124 => f.write_str("remote tool endpoint exited 124: command timed out; remote outcome may be unknown"),
            code => write!(f, "remote tool endpoint exited {code}; verify the remote Rusty supports tool protocol v1"),
        }
    }
}
impl std::error::Error for EndpointExit {}

fn timeout_of(v: &Value, default: i64) -> Result<i64> {
    match v {
        Value::Null => Ok(default),
        Value::Number(n) => {
            n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).ok_or_else(|| anyhow::anyhow!("invalid timeout"))
        }
        Value::String(s) => Ok(s.trim().parse()?),
        _ => bail!("invalid timeout"),
    }
}

/// Runs one tool request in the sandbox and returns its protocol response.
pub fn remote_rpc(remote: &dyn Remote, workspace: &str, request: &Value) -> Result<Value> {
    let encoded = serde_json::to_string(request)?;
    if encoded.len() > MAX_RPC {
        bail!("tool request exceeds 1 MiB");
    }
    let timeout = match request["op"].as_str() {
        Some("execute") => timeout_of(&request["args"]["timeout_secs"], 120)?,
        Some("shell") => timeout_of(&request["timeout"], 120)?,
        _ => 20,
    };
    let timeout = timeout.clamp(1, 600) as u64 + 10;
    // stdin carries JSON. It is quoted as data, never interpreted as shell code.
    let command = format!(
        "cd -- {} && printf '%s' {} | env RUSTY_NO_DOTENV=1 rusty --tool-rpc",
        quote(workspace),
        quote(&encoded)
    );
    let (code, out) = remote.sh(&command, timeout)?;
    if code != 0 {
        return Err(EndpointExit(code).into());
    }
    if out.len() > MAX_RPC {
        bail!("tool response exceeds 1 MiB");
    }
    let mut response: Value = serde_json::from_str(&out).map_err(|_| {
        anyhow::anyhow!("remote endpoint did not return tool protocol JSON; update the remote Rusty binary")
    })?;
    if request["op"] == "describe" && response["ok"] == true {
        if let Some(data) = response["data"].as_object_mut() {
            data.insert("sandbox".into(), json!(remote.id()));
        }
    }
    Ok(response)
}

/// A running bridge. Dropping it stops accepting connections.
pub struct Bridge {
    pub url: String,
    pub token: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Bridge {
    /// Binds 127.0.0.1 on a free port with a fresh 256-bit token.
    pub fn start(remote: Arc<dyn Remote>, workspace: &str) -> Result<Self> {
        if !workspace.starts_with('/') || workspace.contains('\0') {
            bail!("--workspace must be an absolute sandbox path");
        }
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let token = random_hex(32);
        let stop = Arc::new(AtomicBool::new(false));
        let shared =
            Arc::new(Shared { remote, workspace: workspace.into(), token: token.clone(), busy: AtomicUsize::new(0) });
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let shared = shared.clone();
                        std::thread::spawn(move || shared.serve(stream));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(10)),
                }
            }
        });
        Ok(Self { url, token, stop, thread: Some(thread) })
    }

    /// The variables rusty needs for `--tools daytona`.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("RUSTY_TOOL_BRIDGE_URL".into(), self.url.clone()), ("RUSTY_TOOL_BRIDGE_TOKEN".into(), self.token.clone())]
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Shared {
    remote: Arc<dyn Remote>,
    workspace: String,
    token: String,
    busy: AtomicUsize,
}

/// Releases a concurrency slot however the request ends.
struct Slot<'a>(&'a AtomicUsize);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Shared {
    fn acquire(&self) -> Option<Slot<'_>> {
        // A compare-exchange loop: fetch_update is deprecated on newer
        // toolchains and its replacement is missing on older ones.
        let mut n = self.busy.load(Ordering::SeqCst);
        loop {
            if n >= SLOTS {
                return None;
            }
            match self.busy.compare_exchange(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => return Some(Slot(&self.busy)),
                Err(current) => n = current,
            }
        }
    }

    /// One request per connection. Nothing is logged: no commands, tokens or
    /// tool results.
    fn serve(&self, stream: TcpStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
        let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
        let Ok(read_half) = stream.try_clone() else { return };
        let mut reader = BufReader::new(read_half.take(MAX_HEADER as u64));
        let (status, body) = match self.handle(&mut reader) {
            Ok(response) => (200, serde_json::to_vec(&response).unwrap_or_default()),
            Err(status) => (status, Vec::new()),
        };
        respond(stream, status, &body);
    }

    fn handle(&self, reader: &mut BufReader<std::io::Take<TcpStream>>) -> std::result::Result<Value, u16> {
        let mut line = String::new();
        reader.read_line(&mut line).map_err(|_| 400u16)?;
        let mut parts = line.split_whitespace();
        let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let mut auth = String::new();
        let mut length: Option<String> = None;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).map_err(|_| 400u16)? == 0 {
                return Err(400);
            }
            let header = header.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                break;
            }
            if let Some((k, v)) = header.split_once(':') {
                match k.trim().to_ascii_lowercase().as_str() {
                    "authorization" => auth = v.trim().to_string(),
                    "content-length" => length = Some(v.trim().to_string()),
                    _ => {}
                }
            }
        }
        if method != "POST" {
            return Err(501);
        }
        let expected = format!("Bearer {}", self.token);
        if path != "/rpc" || !constant_time_eq(auth.as_bytes(), expected.as_bytes()) {
            return Err(403);
        }
        let length: usize = match length.as_deref().unwrap_or("0").parse() {
            Ok(n) => n,
            Err(_) => return Err(400),
        };
        if length == 0 || length > MAX_RPC {
            return Err(413);
        }
        let Some(_slot) = self.acquire() else { return Err(429) };
        let mut body = vec![0u8; length];
        let result = (|| -> Result<Value> {
            // The header limit no longer applies to the body.
            let buffered = reader.buffer().len().min(length);
            body[..buffered].copy_from_slice(&reader.buffer()[..buffered]);
            reader.consume(buffered);
            reader.get_mut().get_mut().read_exact(&mut body[buffered..])?;
            let request: Value = serde_json::from_slice(&body)?;
            if !request.is_object() {
                bail!("request must be an object");
            }
            remote_rpc(self.remote.as_ref(), &self.workspace, &request)
        })();
        // Transport errors may contain credentials or URLs; keep them generic
        // and never retry an uncertain write.
        Ok(result.unwrap_or_else(|error| {
            // Only our own status diagnostics may cross the bridge, never raw
            // provider errors or command output that could contain secrets.
            let message = match error.downcast_ref::<EndpointExit>() {
                Some(exit) => format!("{exit}. {TRANSPORT_FAILED}"),
                None => TRANSPORT_FAILED.into(),
            };
            json!({"ok": false, "error": message})
        }))
    }
}

fn respond(mut stream: TcpStream, status: u16, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        _ => "Not Implemented",
    };
    let kind = if status == 200 { "application/json" } else { "text/plain" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(body)).and_then(|_| stream.flush());
    // Drain what the client is still sending, so closing doesn't reset the
    // connection before it reads a refusal.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = std::io::copy(&mut (&stream).take((MAX_RPC + MAX_HEADER) as u64), &mut std::io::sink());
}
