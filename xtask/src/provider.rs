//! A paced, deterministic OpenAI-compatible stand-in for the terminal checks.
//! It drives rusty's real file and Bash tools, then streams a long answer, so
//! the panel can be captured without credentials.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::json::{self, quote, Json};

pub struct Provider {
    pub endpoint: String,
    port: u16,
    stop: Arc<AtomicBool>,
}

impl Provider {
    pub fn start() -> Result<Provider> {
        let listener = TcpListener::bind("127.0.0.1:0").context("binding the stand-in provider")?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(stream) = stream {
                    thread::spawn(move || {
                        let _ = serve(stream);
                    });
                }
            }
        });
        Ok(Provider { endpoint: format!("http://127.0.0.1:{port}/v1"), port, stop })
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wake the accept loop
    }
}

pub fn typeahead_input() -> String {
    let prefix = "TYPEAHEAD_RECEIPT ";
    format!("{prefix}{}🙂漢é e\u{301}\nsecond line 🌲 END_UNICODE", "x".repeat(511 - prefix.len()))
}

pub fn typeahead_path_input() -> &'static str {
    "/tmp/rusty-shot/screen.png first café\nsecond line 🌲 PATH_END"
}

fn pause(secs: f64) {
    thread::sleep(Duration::from_secs_f64(secs));
}

fn serve(stream: TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut request = String::new();
    reader.read_line(&mut request)?;
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            }
        }
    }
    if !request.starts_with("POST ") {
        out.write_all(b"HTTP/1.0 501 Unsupported method\r\nConnection: close\r\n\r\n")?;
        return Ok(());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body = json::parse(std::str::from_utf8(&body)?)?;
    let messages = body.get("messages").map(Json::items).unwrap_or_default();
    let role = |m: &Json| m.get("role").and_then(Json::as_str).unwrap_or("").to_string();
    let prompt = messages.iter().rev().find(|m| role(m) == "user").map(text).unwrap_or_default();
    let tools = messages.iter().filter(|m| role(m) == "tool").count();

    out.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")?;
    let mut sse = Sse(out);
    let system = messages.first().map(text).unwrap_or_default();
    let main_side_capture =
        messages.iter().any(|m| role(m) == "user" && matches!(text(m).as_str(), "side-capture" | "summary-capture"))
            && !system.contains("persistent read-only side discussion");
    if system.starts_with("Summarize this read-only side discussion") {
        sse.content("PTY_SIDE_SUMMARY: inspect idempotency before applying a retry.\n", Some("stop"))?;
    } else if prompt == "aside-sentinel" {
        sse.content("ASIDE_ANSWER: this is a context-only answer.\n", Some("stop"))?;
    } else if prompt == "side-slow" {
        pause(0.4);
        sse.content("SIDE_SLOW_ANSWER\n", Some("stop"))?;
    } else if prompt == "side-followup" {
        sse.content("SIDE_FOLLOWUP_ANSWER\n", Some("stop"))?;
    } else if prompt == "side-sentinel" {
        if tools == 0 {
            sse.call("read", "read_file", r#"{"path":"side-fact.txt"}"#)?;
        } else {
            sse.content("SIDE_ANSWER: inspected the local side fact.\n", Some("stop"))?;
        }
    } else if prompt == "side-cancel" {
        for _ in 0..40 {
            pause(0.15);
            sse.content("side reasoning\n", None)?;
        }
    } else if prompt == "aside-capture" || main_side_capture {
        if tools == 0 {
            sse.call(
                "running",
                "bash",
                r#"{"command":"printf started > main-started.txt; sleep 3; printf finished > main-finished.txt"}"#,
            )?;
        } else {
            for i in 0..24 {
                sse.content(&format!("Main line {i:02}: the side answer did not interrupt this task.\n"), None)?;
                pause(0.03);
            }
            sse.content("CAPTURE_COMPLETE\n", Some("stop"))?;
        }
    } else if prompt.starts_with("TYPEAHEAD_RECEIPT") {
        let receipt = if prompt == typeahead_input() { "TYPEAHEAD_EXACT_MATCH" } else { "TYPEAHEAD_MISMATCH" };
        sse.content(&format!("{receipt}\n"), Some("stop"))?;
    } else if prompt.starts_with("/tmp/rusty-shot/screen.png") {
        let expected = format!("{} EDITED", typeahead_path_input());
        let receipt = if prompt == expected { "TYPEAHEAD_PATH_EXACT_MATCH" } else { "TYPEAHEAD_PATH_MISMATCH" };
        sse.content(&format!("{receipt}\n"), Some("stop"))?;
    } else if prompt.contains("Unix") {
        for _ in 0..60 {
            pause(0.15);
            sse.content("A long streamed reply for interruption.\n", None)?;
        }
    } else if prompt.contains("pong") {
        sse.content("pong\n", Some("stop"))?;
    } else if prompt == "approval" {
        pause(0.4);
        if tools == 0 {
            sse.call("approval", "write_file", r#"{"path": "approved.txt", "content": "approved"}"#)?;
        } else {
            sse.content("CAPTURE_COMPLETE\n", Some("stop"))?;
        }
    } else if prompt == "owned-capture" {
        pause(0.1);
        if tools == 0 {
            sse.call(
                "start",
                "bash_start",
                r#"{"command":"sleep 1.5; printf ready > owned-result.txt", "timeout_secs":10}"#,
            )?;
        } else if tools < 8 {
            let handle = messages
                .iter()
                .filter(|m| role(m) == "tool")
                .find_map(|m| {
                    let record = serde_json::from_str::<serde_json::Value>(&text(m)).ok()?;
                    record["id"].as_str().map(str::to_string)
                })
                .context("no executor activity handle")?;
            let wait = if tools < 7 { 0 } else { 3 };
            sse.call(
                &format!("poll-{tools}"),
                "bash_wait",
                &format!("{{\"id\":{},\"wait_secs\":{wait}}}", quote(&handle)),
            )?;
        } else {
            for i in 0..18 {
                sse.content(&format!("Owned capture line {i:02}: the deadline belongs to the executor.\n"), None)?;
                pause(0.1);
            }
            sse.content("CAPTURE_COMPLETE: collected the actual command result.\n", Some("stop"))?;
        }
    } else {
        pause(0.35);
        if tools == 0 {
            sse.call("call0", "write_file", r#"{"path": "capture-note.txt", "content": "verified\n"}"#)?;
        } else if tools == 1 {
            sse.call("call1", "locate", r#"{"query": "where is the capture note verified"}"#)?;
        } else if tools < 12 {
            // Writes as well as prints: rusty stops a turn after five
            // commands that only print text.
            let command = format!("printf 'step {tools:02} verified\\n' | tee -a capture-note.txt");
            sse.call(&format!("call{tools}"), "bash", &format!("{{\"command\": {}}}", quote(&command)))?;
        } else {
            for i in 0..18 {
                sse.content(&format!("Captured line {i:02}: output flows above the geometric panel.\n"), None)?;
                pause(0.1);
            }
            sse.content("CAPTURE_COMPLETE\n", Some("stop"))?;
        }
    }
    sse.0.write_all(b"data: [DONE]\n\n")?;
    sse.0.flush()?;
    Ok(())
}

/// A message's text, whether a plain string or a list of text parts.
fn text(message: &Json) -> String {
    match message.get("content") {
        Some(Json::Str(s)) => s.clone(),
        Some(Json::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Json::as_str)).collect(),
        _ => String::new(),
    }
}

struct Sse(TcpStream);

impl Sse {
    fn send(&mut self, delta: &str, finish: Option<&str>) -> Result<()> {
        let finish = finish.map_or("null".to_string(), quote);
        let event = format!("data: {{\"choices\": [{{\"delta\": {delta}, \"finish_reason\": {finish}}}]}}\n\n");
        self.0.write_all(event.as_bytes())?;
        self.0.flush()?;
        Ok(())
    }

    fn content(&mut self, text: &str, finish: Option<&str>) -> Result<()> {
        self.send(&format!("{{\"content\": {}}}", quote(text)), finish)
    }

    fn call(&mut self, id: &str, name: &str, arguments: &str) -> Result<()> {
        let delta = format!(
            "{{\"tool_calls\": [{{\"index\": 0, \"id\": {}, \"type\": \"function\", \
             \"function\": {{\"name\": {}, \"arguments\": {}}}}}]}}",
            quote(id),
            quote(name),
            quote(arguments)
        );
        self.send(&delta, Some("tool_calls"))
    }
}
