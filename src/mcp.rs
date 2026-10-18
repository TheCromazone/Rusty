//! A minimal MCP client: stdio servers from `.mcp.json` (or `RUSTY_MCP_CONFIG`),
//! their tools offered to the model as `mcp__<server>__<tool>`.
//!
//! Newline-delimited JSON-RPC 2.0 over the server's stdin/stdout, per the MCP
//! stdio transport. Only tools are supported; resources and prompts are not.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Error, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::permissions::Class;

const PROTOCOL: &str = "2025-06-18";
const PREFIX: &str = "mcp__";
/// Longest tool name the model APIs accept.
const MAX_NAME: usize = 64;
const MAX_PAGES: usize = 100;
const MAX_TOOLS: usize = 1000;
const MAX_FRAME: usize = 4 * 1024 * 1024;
const QUEUED_FRAMES: usize = 4;
const HANDSHAKE: Duration = Duration::from_secs(30);
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// What is known about this request, not whether the remote effect succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DispatchOutcome {
    NotStarted,
    Settled,
    Unknown,
}

/// Retained through anyhow contexts for callers that own durable effect markers.
#[derive(Debug)]
pub struct CallFailure {
    pub outcome: DispatchOutcome,
    source: Error,
}

impl CallFailure {
    fn error(outcome: DispatchOutcome, source: impl Into<Error>) -> Error {
        Error::new(Self { outcome, source: source.into() })
    }
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)?;
        if self.outcome == DispatchOutcome::Unknown {
            write!(f, " (MCP outcome unknown; do not replay this call)")?;
        }
        Ok(())
    }
}

impl std::error::Error for CallFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Relative to the tool request, even if connection setup failed first.
pub fn failure_outcome(error: &Error) -> Option<DispatchOutcome> {
    error.downcast_ref::<CallFailure>().map(|failure| failure.outcome)
}

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, Value>,
}

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ServerConfig {
    command: Option<String>,
    url: Option<String>,
    /// `stdio` when given; any other transport is unsupported.
    #[serde(rename = "type")]
    transport: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Startup fails unless this server connects.
    #[serde(default)]
    required: bool,
    /// Startup fails unless the server lists each of these tools.
    #[serde(default)]
    required_tools: Vec<String>,
}

/// One tool, under the name the model sees.
pub struct Tool {
    pub exposed: String,
    name: String,
    description: String,
    schema: Value,
    class: Class,
}

struct Server {
    name: String,
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, String>>,
    reader_stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
    tools: Vec<Tool>,
    /// Features the server offers that rusty doesn't use.
    ignored: Vec<String>,
    command: String,
    config: ServerConfig,
    cwd: PathBuf,
    stopped: bool,
}

/// What became of one configured server, for `--mcp-check` and errors.
#[derive(serde::Serialize)]
pub struct Report {
    server: String,
    required: bool,
    /// `ok`, `skipped` or `failed`.
    status: &'static str,
    /// Why it isn't fully usable, so a harness can tell a capability rusty
    /// declares unsupported (`unsupported_transport`) from a server that
    /// didn't come up (`bad_config`, `start_failed`, `handshake_failed`) and
    /// from tools that weren't found (`discovery_failed`, `missing_tools`).
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    tools: usize,
    missing: Vec<String>,
    /// Server features rusty doesn't use, such as resources and prompts.
    /// A task that needs one is a coverage limitation, not a failure.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    ignored: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl Report {
    /// A required server that didn't connect, or lacks a required tool.
    pub fn blocks(&self) -> bool {
        self.required && (self.status != "ok" || !self.missing.is_empty())
    }
}

/// The connected servers. Dropping it stops them.
#[derive(Default)]
pub struct Mcp {
    servers: Vec<Server>,
}

impl Mcp {
    /// Connects every configured server. A server that fails is skipped with a
    /// warning, so one broken entry never blocks the session, unless it is
    /// marked `required` or names `requiredTools`.
    pub fn load(cwd: &Path) -> Result<Self> {
        let (mcp, reports) = Self::connect(cwd)?;
        for r in &reports {
            if r.blocks() {
                let why = r.error.clone().unwrap_or_else(|| format!("missing tools: {}", r.missing.join(", ")));
                bail!("required MCP server `{}` unavailable: {why}", r.server);
            }
            if let Some(e) = &r.error {
                eprintln!("mcp server `{}` unavailable: {e}", r.server);
            }
        }
        Ok(mcp)
    }

    /// Connects what it can and reports on every configured server.
    pub fn connect(cwd: &Path) -> Result<(Self, Vec<Report>)> {
        let path = match std::env::var_os("RUSTY_MCP_CONFIG") {
            Some(p) => p.into(),
            None => cwd.join(".mcp.json"),
        };
        if !path.exists() {
            return Ok((Self::default(), Vec::new()));
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let config: Config = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let (mut servers, mut reports) = (Vec::new(), Vec::new());
        for (name, raw) in config.servers {
            // Each entry parses alone, so one malformed entry can't hide the rest.
            let parsed = serde_json::from_value::<ServerConfig>(raw.clone());
            let cfg = parsed.as_ref().ok();
            let required = cfg.is_some_and(|c| c.required || !c.required_tools.is_empty())
                || raw["required"].as_bool() == Some(true);
            let mut report = Report {
                server: name.clone(),
                required,
                status: "failed",
                reason: None,
                tools: 0,
                missing: Vec::new(),
                ignored: Vec::new(),
                error: None,
            };
            let started = match (&parsed, cfg.and_then(|c| c.command.as_ref())) {
                (Err(e), _) => Err(("bad_config", anyhow!("bad entry: {e}"))),
                (Ok(c), _) if c.url.is_some() || c.transport.as_deref().is_some_and(|t| t != "stdio") => {
                    report.status = "skipped";
                    let what = c.transport.as_deref().unwrap_or("url");
                    Err(("unsupported_transport", anyhow!("only stdio servers are supported, not `{what}`")))
                }
                (Ok(_), None) => Err(("bad_config", anyhow!("no `command`"))),
                (Ok(c), Some(command)) => Server::start(&name, command, c, cwd),
            };
            match started {
                Ok(server) => {
                    let have: HashSet<&str> = server.tools.iter().map(|t| t.name.as_str()).collect();
                    report.missing = cfg
                        .map(|c| c.required_tools.iter().filter(|t| !have.contains(t.as_str())).cloned().collect())
                        .unwrap_or_default();
                    report.status = "ok";
                    report.reason = (!report.missing.is_empty()).then_some("missing_tools");
                    report.tools = server.tools.len();
                    report.ignored = server.ignored.clone();
                    servers.push(server);
                }
                Err((reason, e)) => {
                    report.reason = Some(reason);
                    report.error = Some(format!("{e:#}"));
                }
            }
            reports.push(report);
        }
        name_tools(&mut servers);
        let mut classes = CLASSES.write().unwrap_or_else(|e| e.into_inner());
        *classes = servers.iter().flat_map(|s| &s.tools).map(|t| (t.exposed.clone(), t.class.clone())).collect();
        Ok((Self { servers }, reports))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// `name (N tools)` for each server, for the banner.
    pub fn summary(&self) -> String {
        self.servers.iter().map(|s| format!("{} ({} tools)", s.name, s.tools.len())).collect::<Vec<_>>().join(", ")
    }

    pub fn is_tool(name: &str) -> bool {
        name.get(..PREFIX.len()).is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
    }

    /// The exposed tool a name means: itself, or the one tool it matches once
    /// separators are ignored, so `mcp__simcloud#get_resource` still works.
    pub fn resolve(&self, name: &str) -> Option<String> {
        let tools = || self.servers.iter().flat_map(|s| &s.tools);
        if tools().any(|t| t.exposed == name) {
            return Some(name.to_string());
        }
        let key = loose(name);
        let mut matches = tools().filter(|t| loose(&t.exposed) == key);
        let found = matches.next()?;
        matches.next().is_none().then(|| found.exposed.clone())
    }

    /// OpenAI-style function definitions for every tool.
    pub fn definitions(&self) -> Vec<Value> {
        self.servers
            .iter()
            .flat_map(|s| &s.tools)
            .map(|t| {
                json!({"type": "function", "function": {
                    "name": t.exposed, "description": t.description, "parameters": t.schema
                }})
            })
            .collect()
    }

    /// Calls a tool by its exposed name and returns its text content. A tool
    /// error comes back as text starting with `error:`, like other tools.
    pub fn call(&mut self, exposed: &str, args: &Value) -> Result<String> {
        self.call_cancellable(exposed, args, &|| {
            crate::signal::poll_keys();
            crate::signal::interrupted()
        })
    }

    /// The callback is checked between bounded transport waits. A failure's
    /// CallFailure outcome must be consulted before clearing an effect marker.
    /// Reconnecting on a later call never replays this call.
    pub fn call_cancellable(&mut self, exposed: &str, args: &Value, cancelled: &dyn Fn() -> bool) -> Result<String> {
        self.call_with_timeout(exposed, args, call_timeout(), cancelled)
    }

    fn call_with_timeout(
        &mut self,
        exposed: &str,
        args: &Value,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<String> {
        if cancelled() {
            return Err(CallFailure::error(DispatchOutcome::NotStarted, anyhow!("MCP call interrupted")));
        }
        let server =
            self.servers.iter_mut().find(|s| s.tools.iter().any(|t| t.exposed == exposed)).ok_or_else(|| {
                CallFailure::error(DispatchOutcome::NotStarted, anyhow!("unknown MCP tool `{exposed}`"))
            })?;
        server.reconnect(cancelled).map_err(|e| CallFailure::error(DispatchOutcome::NotStarted, e))?;
        let name = server.tools.iter().find(|t| t.exposed == exposed).map(|t| t.name.clone()).ok_or_else(|| {
            CallFailure::error(DispatchOutcome::NotStarted, anyhow!("MCP tool disappeared after reconnect"))
        })?;
        let args = if args.is_object() { args.clone() } else { json!({}) };
        let result = server.tool_call(&name, args, timeout, cancelled)?;
        let text: Vec<&str> = result["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| if c["type"] == "text" { c["text"].as_str() } else { None })
            .collect();
        let text = if text.is_empty() { result["structuredContent"].to_string() } else { text.join("\n") };
        let text = crate::tools::cap(text);
        Ok(if result["isError"].as_bool().unwrap_or(false) { format!("error: {text}") } else { text })
    }
}

fn valid_content(value: &Value) -> bool {
    match value["type"].as_str() {
        Some("text") => value["text"].is_string(),
        Some("image" | "audio") => value["data"].is_string() && value["mimeType"].is_string(),
        Some("resource_link") => value["uri"].is_string() && value["name"].is_string(),
        Some("resource") => {
            let resource = &value["resource"];
            resource["uri"].is_string()
                && (resource["text"].is_string() || resource["blob"].is_string())
                && resource.get("mimeType").is_none_or(Value::is_string)
        }
        _ => false,
    }
}

impl Server {
    fn tool_call(&mut self, name: &str, args: Value, timeout: Duration, cancelled: &dyn Fn() -> bool) -> Result<Value> {
        let result =
            self.request_cancellable("tools/call", json!({"name": name, "arguments": args}), timeout, cancelled);
        let result = result.and_then(|value| {
            if !value.is_object()
                || !value.get("content").is_some_and(Value::is_array)
                || value.get("isError").is_some_and(|v| !v.is_boolean())
                || value.get("structuredContent").is_some_and(|v| !v.is_object())
                || value.get("_meta").is_some_and(|v| !v.is_object())
                || value["content"].as_array().is_some_and(|items| items.iter().any(|item| !valid_content(item)))
            {
                return Err(CallFailure::error(DispatchOutcome::Unknown, anyhow!("invalid MCP tools/call result")));
            }
            Ok(value)
        });
        if result.as_ref().err().and_then(failure_outcome) == Some(DispatchOutcome::Unknown) {
            self.stop();
        }
        result
    }

    /// Starts, initializes and lists one server; an error says which stage failed.
    fn start(name: &str, command: &str, cfg: &ServerConfig, cwd: &Path) -> Result<Self, (&'static str, Error)> {
        Self::start_cancellable(name, command, cfg, cwd, &crate::signal::interrupted)
    }

    fn start_cancellable(
        name: &str,
        command: &str,
        cfg: &ServerConfig,
        cwd: &Path,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, (&'static str, Error)> {
        let mut server = Self::spawn(name, command, cfg, cwd).map_err(|e| ("start_failed", e))?;
        server.initialize(cancelled).map_err(|e| ("handshake_failed", e))?;
        server.discover(cancelled).map_err(|e| ("discovery_failed", e))?;
        Ok(server)
    }

    fn reconnect(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
        if !self.stopped {
            return Ok(());
        }
        if cancelled() {
            bail!("MCP call interrupted before reconnect");
        }
        let mut replacement = Self::start_cancellable(&self.name, &self.command, &self.config, &self.cwd, cancelled)
            .map_err(|(_, e)| e)?;
        // The caller authorized the original definitions and permission class.
        // A restarted server must not silently change either of them.
        if replacement.tools.len() != self.tools.len() {
            bail!("MCP server changed its tools after reconnect; reload the session");
        }
        let mut remaining: HashSet<&str> = self.tools.iter().map(|tool| tool.name.as_str()).collect();
        if remaining.len() != self.tools.len() {
            bail!("MCP server has duplicate tool names; reload the session");
        }
        for tool in &mut replacement.tools {
            if !remaining.remove(tool.name.as_str()) {
                bail!("MCP server changed or duplicated a tool after reconnect; reload the session");
            }
            let mut matches = self.tools.iter().filter(|old| old.name == tool.name);
            let old = matches.next().context("MCP server changed a tool after reconnect; reload the session")?;
            if matches.next().is_some()
                || old.schema != tool.schema
                || old.class != tool.class
                || old.description != tool.description
            {
                bail!("MCP server changed a tool after reconnect; reload the session");
            }
            tool.exposed.clone_from(&old.exposed);
        }
        drop(remaining);
        *self = replacement;
        Ok(())
    }

    fn spawn(name: &str, command: &str, cfg: &ServerConfig, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(command)
            .args(&cfg.args)
            .envs(expand_env(&cfg.env)?)
            .current_dir(cwd)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting `{command}`"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        // A reader thread, so a silent server can never block past a deadline.
        let (tx, lines) = mpsc::sync_channel(QUEUED_FRAMES);
        let reader_stop = Arc::new(AtomicBool::new(false));
        let mut server = Self {
            name: name.to_string(),
            child,
            stdin: Some(stdin),
            lines,
            reader_stop: reader_stop.clone(),
            reader: None,
            next_id: 1,
            tools: Vec::new(),
            ignored: Vec::new(),
            command: command.to_string(),
            config: cfg.clone(),
            cwd: cwd.to_path_buf(),
            stopped: false,
        };
        for fd in [server.stdin.as_ref().context("no stdin")?.as_raw_fd(), stdout.as_raw_fd()] {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(std::io::Error::last_os_error()).context("setting MCP pipes nonblocking");
            }
        }
        server.reader = Some(std::thread::spawn(move || {
            let mut reader = BufReader::new(Reader { stdout, stop: reader_stop.clone() });
            'frames: loop {
                let line = read_frame(&mut reader);
                if reader_stop.load(Ordering::Acquire) {
                    break;
                }
                let done = !matches!(&line, Ok(Some(_)));
                let mut message = match line {
                    Ok(Some(line)) => Ok(line),
                    Ok(None) => break,
                    Err(e) => Err(e.to_string()),
                };
                loop {
                    if reader_stop.load(Ordering::Acquire) {
                        break 'frames;
                    }
                    match tx.try_send(message) {
                        Ok(()) => break,
                        Err(mpsc::TrySendError::Disconnected(_)) => break 'frames,
                        Err(mpsc::TrySendError::Full(value)) => {
                            message = value;
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
                if done {
                    break;
                }
            }
        }));
        Ok(server)
    }

    fn initialize(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
        let init = self.request_cancellable(
            "initialize",
            json!({"protocolVersion": PROTOCOL, "capabilities": {},
                   "clientInfo": {"name": "rusty", "version": env!("CARGO_PKG_VERSION")}}),
            HANDSHAKE,
            cancelled,
        )?;
        // Only tools are used; logging and completions need nothing of us.
        let used = ["tools", "logging", "completions", "experimental"];
        self.ignored = init["capabilities"]
            .as_object()
            .into_iter()
            .flat_map(|c| c.keys())
            .filter(|k| !used.contains(&k.as_str()))
            .cloned()
            .collect();
        let mut written = false;
        self.send_cancellable(
            &json!({"jsonrpc":"2.0", "method":"notifications/initialized"}),
            Instant::now() + HANDSHAKE,
            cancelled,
            &mut written,
        )
    }

    fn discover(&mut self, cancelled: &dyn Fn() -> bool) -> Result<()> {
        let name = self.name.clone();
        // `tools/list` is paginated: follow `nextCursor` until it stops,
        // within bounds a looping or endless server can't get past.
        let (mut cursor, mut seen) = (None::<String>, HashSet::new());
        for page in 1.. {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({ "cursor": c }));
            let listed = self.request_cancellable("tools/list", params, HANDSHAKE, cancelled)?;
            for t in listed["tools"].as_array().into_iter().flatten() {
                let Some(tool) = t["name"].as_str().filter(|n| !n.is_empty()) else { continue };
                let mut schema = t["inputSchema"].clone();
                if !schema.is_object() {
                    schema = json!({"type": "object", "properties": {}});
                }
                if self.tools.len() >= MAX_TOOLS {
                    bail!("tools/list exceeded {MAX_TOOLS} tools");
                }
                self.tools.push(Tool {
                    exposed: String::new(),
                    name: tool.to_string(),
                    description: format!("[{name} MCP] {}", t["description"].as_str().unwrap_or("")),
                    schema,
                    class: tool_class(tool, &t["annotations"]),
                });
            }
            cursor = listed["nextCursor"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
            let Some(c) = &cursor else { break };
            if !seen.insert(c.clone()) {
                bail!("tools/list repeated cursor `{c}`");
            }
            if page >= MAX_PAGES || self.tools.len() >= MAX_TOOLS {
                bail!("tools/list still had more after {page} pages and {} tools", self.tools.len());
            }
        }
        Ok(())
    }

    fn send_cancellable(
        &mut self,
        message: &Value,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        written: &mut bool,
    ) -> Result<()> {
        if cancelled() {
            bail!("MCP call interrupted");
        }
        let bytes = encode_frame(message)?;
        let mut rest = bytes.as_slice();
        let stdin = self.stdin.as_mut().context("MCP input is closed")?;
        while !rest.is_empty() {
            if cancelled() {
                bail!("MCP call interrupted");
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("MCP input timed out");
            }
            match stdin.write(rest) {
                Ok(0) => bail!("the MCP server closed its input"),
                Ok(n) => {
                    *written = true;
                    rest = &rest[n..];
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: stdin.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                    let status = unsafe { libc::poll(&mut fd, 1, left.min(CANCEL_POLL).as_millis().max(1) as i32) };
                    if status < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        return Err(std::io::Error::last_os_error()).context("waiting for MCP input");
                    }
                }
                Err(e) => return Err(e).context("the MCP server closed its input"),
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_cancellable(method, params, timeout, &crate::signal::interrupted)
    }

    fn request_cancellable(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let deadline = Instant::now() + timeout;
        let mut written = false;
        self.send_cancellable(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            deadline,
            cancelled,
            &mut written,
        )
        .map_err(|e| {
            CallFailure::error(if written { DispatchOutcome::Unknown } else { DispatchOutcome::NotStarted }, e)
        })?;
        loop {
            if cancelled() {
                return Err(CallFailure::error(DispatchOutcome::Unknown, anyhow!("MCP call interrupted")));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(CallFailure::error(
                    DispatchOutcome::Unknown,
                    anyhow!("{method} timed out after {}s", timeout.as_secs()),
                ));
            }
            let line = match self.lines.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => {
                    return Err(CallFailure::error(DispatchOutcome::Unknown, anyhow!("MCP output rejected: {error}")))
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(CallFailure::error(
                        DispatchOutcome::Unknown,
                        anyhow!("the MCP server `{}` exited", self.name),
                    ))
                }
            };
            // Skip notifications, server requests and stray output.
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg["id"].as_u64() != Some(id) || msg.get("method").is_some() {
                continue;
            }
            if msg["jsonrpc"] != "2.0" || msg.get("result").is_some() == msg.get("error").is_some() {
                return Err(CallFailure::error(DispatchOutcome::Unknown, anyhow!("invalid MCP reply envelope")));
            }
            if let Some(err) = msg.get("error") {
                if !err["code"].is_i64() || !err["message"].is_string() {
                    return Err(CallFailure::error(DispatchOutcome::Unknown, anyhow!("invalid MCP error reply")));
                }
                return Err(CallFailure::error(
                    DispatchOutcome::Settled,
                    anyhow!("{method}: {}", err["message"].as_str().unwrap_or("error")),
                ));
            }
            return Ok(msg["result"].clone());
        }
    }
}

/// Polling permits shutdown even if an escaped descendant retains stdout.
struct Reader {
    stdout: std::process::ChildStdout,
    stop: Arc<AtomicBool>,
}
impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "MCP reader stopped"));
            }
            match self.stdout.read(bytes) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: self.stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    let status = unsafe { libc::poll(&mut fd, 1, 50) };
                    if status < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                result => return result,
            }
        }
    }
}

/// Cap serialization while writing, including JSON escape expansion.
fn encode_frame(message: &Value) -> Result<Vec<u8>> {
    struct Frame(Vec<u8>);
    impl Write for Frame {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > (MAX_FRAME - 1).saturating_sub(self.0.len()) {
                return Err(std::io::Error::other("MCP request exceeds 4 MiB"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut frame = Frame(Vec::new());
    serde_json::to_writer(&mut frame, message)?;
    frame.0.push(b'\n');
    Ok(frame.0)
}

/// Limit before allocating or parsing, including output without a newline.
fn read_frame(reader: &mut impl BufRead) -> Result<Option<String>> {
    let mut bytes = Vec::new();
    reader.take((MAX_FRAME + 1) as u64).read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_FRAME {
        bail!("MCP frame exceeds 4 MiB");
    }
    Ok(Some(String::from_utf8(bytes).context("MCP output is not UTF-8")?))
}

impl Server {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.reader_stop.store(true, Ordering::Release);
        // A launcher may exit before its server. Reap the child and stop its
        // owned process group so descendants cannot retain stdout/readers.
        // Keep the unreaped Child as the PID/group anchor until after signalling.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        self.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        self.lines = mpsc::channel().1;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Lowercase letters and digits only.
fn loose(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

fn call_timeout() -> Duration {
    let secs = std::env::var("RUSTY_MCP_TIMEOUT").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    Duration::from_secs(secs)
}

/// `mcp__server__tool`, limited to the characters tool names allow.
fn exposed_name(server: &str, tool: &str) -> String {
    let clean =
        |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect::<String>();
    format!("{PREFIX}{}__{}", clean(server), clean(tool))
}

/// Gives every tool a distinct exposed name. A name that is too long, or
/// that another tool also cleans to (`a.b` and `a_b`), is cut short and ends
/// in a hash of its server and tool, so it is the same on every run whatever
/// else is listed. Any tool still clashing is dropped rather than shadowed.
fn name_tools(servers: &mut [Server]) {
    let plain: Vec<Vec<String>> =
        servers.iter().map(|s| s.tools.iter().map(|t| exposed_name(&s.name, &t.name)).collect()).collect();
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for n in plain.iter().flatten() {
        *uses.entry(n).or_default() += 1;
    }
    let mut taken = HashSet::new();
    for (server, names) in servers.iter_mut().zip(&plain) {
        let mut tools = std::mem::take(&mut server.tools);
        for (tool, plain) in tools.iter_mut().zip(names) {
            tool.exposed = if plain.len() <= MAX_NAME && uses[plain.as_str()] == 1 {
                plain.clone()
            } else {
                let hash = rusty::fnv(&format!("{}\0{}", server.name, tool.name)) as u32;
                format!("{}_{hash:08x}", &plain[..plain.len().min(MAX_NAME - 9)])
            };
        }
        tools.retain(|t| {
            let fresh = taken.insert(t.exposed.clone());
            if !fresh {
                eprintln!("mcp tool `{}` of `{}` dropped: its name clashes", t.name, server.name);
            }
            fresh
        });
        server.tools = tools;
    }
}

/// Each loaded tool's permission class, by exposed name. Policy checks see
/// only a name, and an exposed name can be cut short or hashed, so the class
/// is decided once from the tool's real name and kept here.
static CLASSES: RwLock<BTreeMap<String, Class>> = RwLock::new(BTreeMap::new());

/// The permission class of an exposed MCP tool name.
pub fn classify(exposed: &str) -> Class {
    let known = CLASSES.read().unwrap_or_else(|e| e.into_inner()).get(exposed).cloned();
    known.unwrap_or_else(|| verb_class(exposed.rsplit("__").next().unwrap_or(exposed)))
}

/// A tool's class from its real name, raised (never lowered) by the server's
/// annotations: `destructiveHint: true` makes it a delete, and
/// `readOnlyHint: false` stops a read-sounding name running unasked. Hints
/// only ever add caution, since a server could claim anything.
fn tool_class(name: &str, annotations: &Value) -> Class {
    let class = verb_class(name);
    if annotations["destructiveHint"] == json!(true) && !matches!(class, Class::Destructive(_)) {
        return Class::Destructive(format!("MCP tool {name} is marked destructive"));
    }
    if annotations["readOnlyHint"] == json!(false) && class == Class::ReadOnly {
        return Class::Risky(format!("MCP tool {name} is marked as changing something"));
    }
    class
}

/// The verb a tool name starts or ends with decides its permission class:
/// reads run freely, deletes need a person, everything else counts as a change.
fn verb_class(tool: &str) -> Class {
    let tool = tool.to_ascii_lowercase();
    let words: Vec<&str> = tool.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
    if has(&["delete", "destroy", "purge", "drop", "remove", "revoke", "wipe", "truncate"]) {
        return Class::Destructive(format!("MCP tool {tool} deletes"));
    }
    const READS: &[&str] = &[
        "get",
        "list",
        "describe",
        "read",
        "search",
        "find",
        "status",
        "logs",
        "log",
        "metrics",
        "show",
        "whoami",
        "simulate",
        "trace",
        "audit",
        "incidents",
        "diff",
        "plan",
        "drift",
        "compare",
        "explain",
        "query",
        "inspect",
        "pending",
        "timeline",
        "history",
        "fetch",
        "view",
        "check",
        "health",
        "kinds",
        "access",
    ];
    if words.first().is_some_and(|w| READS.contains(w)) || words.last().is_some_and(|w| READS.contains(w)) {
        return Class::ReadOnly;
    }
    Class::Risky(format!("MCP tool {tool} may change something"))
}

/// `"NAME": "${VAR}"` takes VAR from rusty's own environment, so a shared
/// `.mcp.json` can name a credential without containing it. Other values are
/// passed as written.
fn expand_env(env: &BTreeMap<String, String>) -> Result<Vec<(String, String)>> {
    let mut out = Vec::with_capacity(env.len());
    for (key, value) in env {
        let reference = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')).filter(|v| env_name(v));
        let value = match reference {
            Some(var) => std::env::var(var).map_err(|_| anyhow!("environment variable {var} for {key} is not set"))?,
            None => value.clone(),
        };
        out.push((key.clone(), value));
    }
    Ok(out)
}

fn env_name(s: &str) -> bool {
    s.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Manage the existing MCP manifest without starting anything on add/list.
/// Preserve unknown fields and existing env values; never echo credentials.
pub fn manage(cwd: &Path, policy: &crate::permissions::Policy, args: &[String]) -> Result<String> {
    use std::io::Write;
    let path =
        std::env::var_os("RUSTY_MCP_CONFIG").map(std::path::PathBuf::from).unwrap_or_else(|| cwd.join(".mcp.json"));
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "help" {
        return Ok("MCP: /integrations mcp list | add NAME [--env VAR]... [--] COMMAND [ARGS...] | remove NAME | check. Add saves an optional stdio server; it starts only on check or the next session. --env VAR saves a reference to VAR, read from the environment when the server starts, never its value. Arguments that look like secrets are refused. Use the CLI form, rusty --integrations mcp add NAME -- [--env VAR]... COMMAND [ARGS...], for arguments containing spaces. Existing .mcp.json / RUSTY_MCP_CONFIG remains the source of truth. Direct HTTP transport is not supported; an explicitly configured stdio bridge can supply it.".into());
    }
    if action == "check" {
        anyhow::ensure!(args.len() == 1, "usage: /integrations mcp check");
        let (_, reports) = Mcp::connect(cwd)?;
        let all_connected = reports.iter().all(|r| r.status == "ok" && r.missing.is_empty());
        let output =
            rusty::privacy::redact(&serde_json::json!({"all_connected":all_connected,"servers":reports}).to_string()).0;
        anyhow::ensure!(all_connected, "MCP health check failed: {output}");
        return Ok(output);
    }
    // Lock the containing directory: stable across profiles and path aliases,
    // with no persistent lock artifact in a user's repository. Rename remains
    // atomic for readers; cooperating writers serialize read/modify/write.
    let _lock = if matches!(action, "add" | "remove") {
        let verdict = policy.decide("write_file", &serde_json::json!({"path":path}), cwd);
        anyhow::ensure!(
            matches!(verdict, crate::permissions::Verdict::Allow),
            "MCP config change refused by permissions: {verdict:?}"
        );
        let parent = path.parent().context("MCP config directory missing")?;
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::File::open(parent)?;
        anyhow::ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "cannot lock MCP configuration directory"
        );
        Some(lock)
    } else {
        None
    };
    let mut value: Value = if path.exists() {
        serde_json::from_slice(&std::fs::read(&path)?).context("invalid MCP configuration; left unchanged")?
    } else {
        serde_json::json!({"mcpServers":{}})
    };
    anyhow::ensure!(value.is_object(), "MCP configuration must be an object");
    if value.get("mcpServers").is_none() {
        value["mcpServers"] = serde_json::json!({});
    }
    let servers = value["mcpServers"].as_object_mut().context("mcpServers must be an object")?;
    if action == "list" {
        anyhow::ensure!(args.len() <= 1, "usage: /integrations mcp list");
        let rows: Vec<Value> = servers
            .iter()
            .map(|(name, cfg)| {
                serde_json::json!({"name":name,
            "transport": if cfg.get("url").is_some() { "unsupported HTTP" } else { "stdio" },
            "required":cfg["required"].as_bool().unwrap_or(false),
            "required_tools":cfg["requiredTools"].as_array().map(Vec::len).unwrap_or(0),
            "authentication":"not checked"})
            })
            .collect();
        return Ok(serde_json::json!({"servers":rows}).to_string());
    }
    anyhow::ensure!(matches!(action, "add" | "remove"), "unknown MCP action; use list, add, remove, check or help");
    let name = args.get(1).context("MCP server name required")?;
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "server name must contain only letters, digits, _ or -"
    );
    match action {
        "add" => {
            let usage = "usage: /integrations mcp add NAME [--env VAR]... [--] COMMAND [ARGS...]";
            anyhow::ensure!(
                !servers.contains_key(name),
                "MCP server already exists; remove it explicitly before replacing it"
            );
            let mut at = if args.get(2).is_some_and(|a| a == "--") { 3 } else { 2 };
            let mut env = serde_json::Map::new();
            while args.get(at).is_some_and(|a| a == "--env") {
                let var = args.get(at + 1).context(usage)?;
                anyhow::ensure!(env_name(var), "--env takes an environment variable name such as GITHUB_TOKEN");
                env.insert(var.clone(), Value::String(format!("${{{var}}}")));
                at += 2;
            }
            if args.get(at).is_some_and(|a| a == "--") {
                at += 1;
            }
            let command = args.get(at).context(usage)?;
            anyhow::ensure!(!command.trim().is_empty(), "MCP server command required");
            // .mcp.json is often committed, and arguments also land in shell
            // history and process listings.
            anyhow::ensure!(
                rusty::privacy::redact(&args[at..].join(" ")).1 == 0,
                "an argument looks like a secret; MCP configuration is often committed, so pass it with --env VAR instead"
            );
            // Optional until it has been seen to start: a required server that
            // can't start stops every later session, including teammates'.
            let mut entry = serde_json::json!({"command": command, "args": &args[at + 1..], "required": false});
            if !env.is_empty() {
                entry["env"] = Value::Object(env);
            }
            servers.insert(name.clone(), entry);
        }
        _ => {
            anyhow::ensure!(args.len() == 2, "usage: /integrations mcp remove NAME");
            anyhow::ensure!(servers.remove(name).is_some(), "MCP server not configured");
        }
    }
    // A shared project file keeps the mode it already has.
    let existing = if path.exists() {
        let meta = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(meta.file_type().is_file(), "MCP config must be a regular file, not a symlink");
        Some(meta.permissions())
    } else {
        None
    };
    let parent = path.parent().context("MCP config directory missing")?;
    std::fs::create_dir_all(parent)?;
    let pending = path.with_extension("json.pending");
    let mut file = rusty::privacy::private_file(&pending, false)?;
    // Unlike transcript persistence, a config edit must not scrub an existing
    // server's secret value into a nonfunctional placeholder.
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.write_all(b"\n")?;
    if let Some(permissions) = existing {
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    std::fs::rename(&pending, &path)?;
    let note = if action == "add" {
        format!(
            ". It is optional; once `/integrations mcp check` passes, set \"required\": true on it in {} to stop sessions starting without it",
            path.display()
        )
    } else {
        String::new()
    };
    Ok(format!("MCP server {name} {action} saved; existing sessions keep their current connections{note}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusty-mcp-outcome-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn outcome_fixture(path: &Path) -> Mcp {
        let script = r#"
import json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
with (root / 'starts').open('a') as f: f.write(str(os.getpid()) + '\n')
for line in sys.stdin:
    message = json.loads(line)
    method = message.get('method')
    if 'id' not in message: continue
    reply = {'jsonrpc': '2.0', 'id': message['id']}
    if method == 'initialize':
        reply['result'] = {'capabilities': {}}
    elif method == 'tools/list':
        tool = {'name': 'apply', 'inputSchema': {'type': 'object'}}
        if (root / 'changed').exists(): tool['annotations'] = {'destructiveHint': True}
        other = tool if (root / 'duplicate').exists() else {'name': 'read', 'inputSchema': {'type': 'object'}}
        reply['result'] = {'tools': [tool, other]}
    else:
        label = message['params']['arguments'].get('label', 'ok')
        with (root / 'effects').open('a') as f: f.write(label + '\n')
        if label == 'wait':
            (root / 'ready').write_text('effect completed; reply withheld')
            time.sleep(60)
            continue
        if label == 'rpc-error':
            reply['error'] = {'code': -32000, 'message': 'fixture refusal'}
        else:
            reply['result'] = {'content': [{'type': 'text', 'text': 'ok'}]}
            if label == 'malformed': reply['jsonrpc'] = 'wrong'
            if label == 'result-null': reply['result'] = None
            if label == 'result-string': reply['result'] = 'invalid'
            if label == 'content-bad': reply['result']['content'] = 'invalid'
            if label == 'block-bad': reply['result']['content'] = [None]
            if label == 'is-error-bad': reply['result']['isError'] = 'invalid'
            if label == 'structured-bad': reply['result']['structuredContent'] = 'invalid'
            if label == 'meta-bad': reply['result']['_meta'] = 'invalid'
            if label == 'non-text':
                reply['result'] = {'content': [{'type':'audio', 'data':'AA==', 'mimeType':'audio/wav'}], 'structuredContent': {'ok': True}}
    print(json.dumps(reply), flush=True)
"#;
        let cfg = ServerConfig {
            args: vec!["-c".into(), script.into(), path.display().to_string()],
            ..ServerConfig::default()
        };
        let server = Server::start("outcome", "python3", &cfg, path).unwrap_or_else(|(_, e)| panic!("{e:#}"));
        let mut servers = vec![server];
        name_tools(&mut servers);
        Mcp { servers }
    }

    fn assert_stopped(server: &Server, pid: u32, reader: &Arc<AtomicBool>) {
        assert!(server.stopped && server.stdin.is_none() && server.reader.is_none());
        assert_eq!(Arc::strong_count(reader), 2, "reader thread still owns its cancellation state");
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "owned MCP child was not reaped");
    }

    #[test]
    fn a_cancelled_effect_is_unknown_reaped_and_never_replayed_on_reconnect() {
        let scratch = Scratch::new();
        let mut mcp = outcome_fixture(&scratch.0);
        let name = names(&mcp.servers)[0].clone();
        let pid = mcp.servers[0].child.id();
        let reader = mcp.servers[0].reader_stop.clone();
        let error = mcp
            .call_with_timeout(&name, &json!({"label":"wait"}), Duration::from_secs(5), &|| {
                scratch.0.join("ready").exists()
            })
            .unwrap_err();
        assert!(scratch.0.join("ready").exists(), "cancellation must follow the actual effect");
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown));
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        assert_stopped(&mcp.servers[0], pid, &reader);
        assert_eq!(std::fs::read_to_string(scratch.0.join("effects")).unwrap(), "wait\n");
        assert_eq!(mcp.call_cancellable(&name, &json!({"label":"next"}), &|| false).unwrap(), "ok");
        assert_eq!(std::fs::read_to_string(scratch.0.join("effects")).unwrap(), "wait\nnext\n");
        assert_eq!(std::fs::read_to_string(scratch.0.join("starts")).unwrap().lines().count(), 2);
    }

    #[test]
    fn cancellation_before_writing_is_not_started_and_replies_settle() {
        let scratch = Scratch::new();
        let mut mcp = outcome_fixture(&scratch.0);
        let name = names(&mcp.servers)[0].clone();
        let error = mcp.call_cancellable(&name, &json!({}), &|| true).unwrap_err().context("outer context");
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::NotStarted));
        assert!(!scratch.0.join("effects").exists());
        assert_eq!(mcp.call_cancellable(&name, &json!({}), &|| false).unwrap(), "ok");
        assert_eq!(mcp.call_cancellable(&name, &json!({"label":"non-text"}), &|| false).unwrap(), r#"{"ok":true}"#);
        let error = mcp.call_cancellable(&name, &json!({"label":"rpc-error"}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Settled));
        assert!(!mcp.servers[0].stopped);
        assert_eq!(mcp.call_cancellable(&name, &json!({}), &|| false).unwrap(), "ok");
        assert_eq!(std::fs::read_to_string(scratch.0.join("starts")).unwrap().lines().count(), 1);
    }

    #[test]
    fn timeout_and_malformed_replies_are_unknown_and_poison_the_connection() {
        for label in ["wait", "malformed"] {
            let scratch = Scratch::new();
            let mut mcp = outcome_fixture(&scratch.0);
            let pid = mcp.servers[0].child.id();
            let reader = mcp.servers[0].reader_stop.clone();
            let error = mcp.servers[0]
                .tool_call("apply", json!({"label":label}), Duration::from_secs(1), &|| false)
                .unwrap_err();
            assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown));
            assert_stopped(&mcp.servers[0], pid, &reader);
        }
    }

    #[test]
    fn a_reconnected_server_cannot_change_the_authorized_tool_definition() {
        let scratch = Scratch::new();
        let mut mcp = outcome_fixture(&scratch.0);
        let name = names(&mcp.servers)[0].clone();
        let error = mcp.call_cancellable(&name, &json!({"label":"malformed"}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown));
        std::fs::write(scratch.0.join("changed"), "change annotation").unwrap();
        let error = mcp.call_cancellable(&name, &json!({"label":"should-not-run"}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::NotStarted));
        assert_eq!(std::fs::read_to_string(scratch.0.join("effects")).unwrap(), "malformed\n");
        assert!(mcp.servers[0].stopped);
    }

    #[test]
    fn malformed_tool_results_cannot_settle_an_effect() {
        for label in
            ["result-null", "result-string", "content-bad", "block-bad", "is-error-bad", "structured-bad", "meta-bad"]
        {
            let scratch = Scratch::new();
            let mut mcp = outcome_fixture(&scratch.0);
            let name = names(&mcp.servers)[0].clone();
            let pid = mcp.servers[0].child.id();
            let reader = mcp.servers[0].reader_stop.clone();
            let error = mcp.call_cancellable(&name, &json!({"label":label}), &|| false).unwrap_err();
            assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown), "{label}");
            assert_stopped(&mcp.servers[0], pid, &reader);
        }
    }

    #[test]
    fn reconnect_requires_a_bijection_and_never_sends_an_empty_tool_name() {
        let scratch = Scratch::new();
        let mut mcp = outcome_fixture(&scratch.0);
        let original = names(&mcp.servers);
        let error = mcp.call_cancellable(&original[0], &json!({"label":"malformed"}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown));
        std::fs::write(scratch.0.join("duplicate"), "A,B became A,A").unwrap();
        let error = mcp.call_cancellable(&original[1], &json!({"label":"should-not-run"}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::NotStarted));
        assert_eq!(std::fs::read_to_string(scratch.0.join("effects")).unwrap(), "malformed\n");
    }

    #[test]
    fn failed_reconnect_does_not_start_the_tool() {
        let scratch = Scratch::new();
        let mut mcp = outcome_fixture(&scratch.0);
        let name = names(&mcp.servers)[0].clone();
        mcp.servers[0].stop();
        mcp.servers[0].command = "/nonexistent-rusty-mcp-fixture".into();
        let error = mcp.call_cancellable(&name, &json!({}), &|| false).unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::NotStarted));
        assert!(!scratch.0.join("effects").exists());
    }

    #[test]
    fn cancellation_while_input_is_full_preserves_partial_write_uncertainty() {
        let cfg = ServerConfig { args: vec!["-c".into(), "sleep 60".into()], ..ServerConfig::default() };
        let mut server = Server::spawn("blocked-cancel", "/bin/sh", &cfg, Path::new(".")).unwrap();
        let pid = server.child.id();
        let reader = server.reader_stop.clone();
        let fd = server.stdin.as_ref().unwrap().as_raw_fd();
        let error = server
            .tool_call("apply", json!({"data":"x".repeat(1024*1024)}), Duration::from_secs(5), &|| {
                let mut writable = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
                unsafe { libc::poll(&mut writable, 1, 0) == 0 }
            })
            .unwrap_err();
        assert_eq!(failure_outcome(&error), Some(DispatchOutcome::Unknown));
        assert!(error.to_string().contains("interrupted"), "{error:#}");
        assert_stopped(&server, pid, &reader);
    }

    fn server(name: &str, tools: &[&str]) -> Server {
        let mut child = Command::new("true").stdin(Stdio::piped()).spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let tools = tools
            .iter()
            .map(|t| Tool {
                exposed: String::new(),
                name: t.to_string(),
                description: String::new(),
                schema: json!({}),
                class: verb_class(t),
            })
            .collect();
        Server {
            name: name.into(),
            child,
            stdin: Some(stdin),
            lines: mpsc::channel().1,
            reader_stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            next_id: 1,
            tools,
            ignored: Vec::new(),
            command: "true".into(),
            config: ServerConfig::default(),
            cwd: PathBuf::from("."),
            stopped: false,
        }
    }

    fn names(servers: &[Server]) -> Vec<String> {
        servers.iter().flat_map(|s| &s.tools).map(|t| t.exposed.clone()).collect()
    }

    #[test]
    fn names_are_prefixed_safe_and_distinct() {
        let long = "t".repeat(80);
        let mut servers =
            vec![server("simcloud", &["secret.access", "a.b", "a_b", &long]), server("simcloud.x", &["get"])];
        name_tools(&mut servers);
        let got = names(&servers);
        assert_eq!(got[0], "mcp__simcloud__secret_access");
        assert_eq!(got[4], "mcp__simcloud_x__get");
        // `a.b` and `a_b` clean to the same name, so both carry a hash.
        assert!(got[1].starts_with("mcp__simcloud__a_b_") && got[2].starts_with("mcp__simcloud__a_b_"));
        assert_ne!(got[1], got[2]);
        assert!(got.iter().all(|n| n.len() <= MAX_NAME));
        assert_eq!(got.iter().collect::<HashSet<_>>().len(), got.len());
        // The same tool gets the same name whatever else is listed.
        let mut alone = vec![server("simcloud", &["a.b"])];
        name_tools(&mut alone);
        assert_eq!(names(&alone), ["mcp__simcloud__a_b"]);
        let mut again = vec![server("simcloud", &["a_b", "a.b"])];
        name_tools(&mut again);
        assert_eq!(names(&again)[1], got[1]);
        assert!(Mcp::is_tool("mcp__a__b") && !Mcp::is_tool("bash"));
    }

    #[test]
    fn a_duplicate_listing_is_dropped_not_shadowed() {
        let mut servers = vec![server("s", &["get", "get"])];
        name_tools(&mut servers);
        assert_eq!(servers[0].tools.len(), 1);
    }

    #[test]
    fn verbs_decide_the_permission_class() {
        assert_eq!(verb_class("get"), Class::ReadOnly);
        assert_eq!(verb_class("simulate_access"), Class::ReadOnly);
        assert_eq!(verb_class("pending_iam_changes"), Class::ReadOnly);
        assert!(matches!(verb_class("delete"), Class::Destructive(_)));
        assert!(matches!(verb_class("kv_put"), Class::Risky(_)));
        assert!(matches!(verb_class("deploy"), Class::Risky(_)));
    }

    #[test]
    fn the_real_name_decides_even_when_the_exposed_one_is_cut() {
        let tool = format!("{}_delete", "x".repeat(70));
        let mut servers = vec![server("s", &[&tool])];
        name_tools(&mut servers);
        let exposed = &servers[0].tools[0].exposed;
        assert!(!exposed.contains("delete"), "{exposed}");
        assert!(matches!(verb_class(exposed), Class::Risky(_)), "the cut name alone would hide the delete");
        CLASSES.write().unwrap().insert(exposed.clone(), servers[0].tools[0].class.clone());
        assert!(matches!(classify(exposed), Class::Destructive(_)));
    }

    #[test]
    fn annotations_only_add_caution() {
        assert!(matches!(tool_class("apply", &json!({"destructiveHint": true})), Class::Destructive(_)));
        assert!(matches!(tool_class("get_or_create", &json!({"readOnlyHint": false})), Class::Risky(_)));
        assert!(matches!(tool_class("deploy", &json!({"readOnlyHint": true})), Class::Risky(_)));
        assert!(matches!(tool_class("drop", &json!({"destructiveHint": false})), Class::Destructive(_)));
        assert_eq!(tool_class("get", &json!({})), Class::ReadOnly);
    }
    #[test]
    fn env_references_come_from_the_environment_and_literals_pass_through() {
        let env = BTreeMap::from([("A".to_string(), "${PATH}".to_string()), ("B".to_string(), "plain".to_string())]);
        let expanded = expand_env(&env).unwrap();
        assert_eq!(
            expanded,
            vec![("A".to_string(), std::env::var("PATH").unwrap()), ("B".to_string(), "plain".to_string())]
        );
        let missing = BTreeMap::from([("T".to_string(), "${RUSTY_TEST_UNSET_VARIABLE}".to_string())]);
        let err = expand_env(&missing).unwrap_err().to_string();
        assert!(err.contains("RUSTY_TEST_UNSET_VARIABLE"), "{err}");
    }

    #[test]
    fn mcp_add_saves_optional_servers_with_env_references_and_refuses_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rusty-mcp-manage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".mcp.json");
        std::fs::write(&path, "{\"mcpServers\":{}}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let policy = crate::permissions::Policy::new(crate::permissions::Mode::Yolo);
        let run = |args: &[&str]| manage(&dir, &policy, &args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let out = run(&["add", "gh", "--env", "GITHUB_TOKEN", "--", "github-mcp", "stdio"]).unwrap();
        assert!(out.contains("\"required\": true"), "{out}");
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved["mcpServers"]["gh"],
            json!({"command": "github-mcp", "args": ["stdio"], "required": false, "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}})
        );
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o644);
        for args in [
            vec!["add", "leak", "server", "--token=abcdefghijklmnop"],
            vec!["add", "leak", "server", "--password", "hunter2hunter2"],
            vec!["add", "leak", "server", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"],
            vec!["add", "leak", "--env", "NOT-A-NAME", "server"],
            vec!["add", "leak", "--env"],
        ] {
            assert!(run(&args[..]).is_err(), "{args:?}");
        }
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after, saved);
        assert!(!dir.join(".mcp.json.pending").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_and_unterminated_mcp_frames_are_bounded_before_parsing() {
        for terminated in [false, true] {
            let mut bytes = vec![b'x'; MAX_FRAME + 1];
            if terminated {
                bytes.push(b'\n');
            }
            let mut reader = std::io::Cursor::new(bytes);
            assert!(read_frame(&mut reader).unwrap_err().to_string().contains("4 MiB"));
            assert_eq!(reader.position(), (MAX_FRAME + 1) as u64);
        }
        let mut reader = std::io::Cursor::new(b"one\ntwo\n");
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("one\n"));
        assert_eq!(read_frame(&mut reader).unwrap().as_deref(), Some("two\n"));
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn queued_notifications_cannot_extend_an_expired_request_deadline() {
        let cfg = ServerConfig { args: vec!["-c".into(), "sleep 60".into()], ..ServerConfig::default() };
        let mut server = Server::spawn("deadline", "/bin/sh", &cfg, Path::new(".")).unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(json!({"method":"notifications/progress"}).to_string())).unwrap();
        tx.send(Ok(json!({"id":1,"result":{"late":true}}).to_string())).unwrap();
        server.lines = rx;
        let error = server.request("probe", json!({}), Duration::ZERO).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        let pid = server.child.id();
        drop(server);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "MCP child was not reaped");
    }
    #[test]
    fn closing_a_launcher_stops_its_owned_descendants() {
        let path = std::env::temp_dir().join(format!("rusty-mcp-child-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = ServerConfig {
            args: vec![
                "-c".into(),
                "sleep 60 & echo $! > \"$1\"; wait".into(),
                "fixture".into(),
                path.display().to_string(),
            ],
            ..ServerConfig::default()
        };
        let server = Server::spawn("launcher", "/bin/sh", &cfg, Path::new(".")).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        let child = loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < end, "launcher did not publish child PID");
            std::thread::sleep(Duration::from_millis(5));
        };
        drop(server);
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let out = Command::new("ps").args(["-o", "stat=", "-p", &child.to_string()]).output().unwrap();
            let state = String::from_utf8_lossy(&out.stdout);
            // An orphan zombie awaits init's reap but retains no live process.
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                break;
            }
            assert!(Instant::now() < end, "MCP descendant still running: {state}");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn an_unread_mcp_input_cannot_hold_a_request_past_its_deadline() {
        let cfg = ServerConfig { args: vec!["-c".into(), "sleep 3".into()], ..ServerConfig::default() };
        let mut server = Server::spawn("blocked-input", "/bin/sh", &cfg, Path::new(".")).unwrap();
        let error =
            server.request("probe", json!({"data":"x".repeat(1024 * 1024)}), Duration::from_millis(20)).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }
    #[test]
    fn a_real_notification_flood_respects_the_request_deadline_and_drops_cleanly() {
        let cfg = ServerConfig {
            args: vec![
                "-c".into(),
                r#"while :; do printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/progress"}'; done"#.into(),
            ],
            ..ServerConfig::default()
        };
        let mut server = Server::spawn("flood", "/bin/sh", &cfg, Path::new(".")).unwrap();
        let first = server.lines.recv_timeout(Duration::from_secs(30)).unwrap().unwrap();
        assert!(first.contains("notifications/progress"));
        let pid = server.child.id();
        let (done, stop) = mpsc::channel();
        // A regression must fail rather than leave an infinite fixture behind.
        let guard = std::thread::spawn(move || {
            if matches!(stop.recv_timeout(Duration::from_secs(60)), Err(mpsc::RecvTimeoutError::Timeout)) {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        });
        let result = server.request("probe", json!({}), Duration::from_millis(50));
        let _ = done.send(());
        guard.join().unwrap();
        assert!(result.unwrap_err().to_string().contains("timed out"));
        drop(server);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
    #[test]
    fn outbound_json_is_capped_during_escape_expansion() {
        assert!(encode_frame(&json!({"data":"\u{0001}".repeat(MAX_FRAME / 4)}))
            .unwrap_err()
            .to_string()
            .contains("4 MiB"));
        let frame = encode_frame(&json!({"ok":true})).unwrap();
        assert!(frame.ends_with(b"\n"));
        assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap(), json!({"ok":true}));
    }

    #[test]
    fn drop_joins_reader_when_an_escaped_descendant_holds_stdout_open() {
        struct Escaped(u32);
        impl Drop for Escaped {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0 as i32, libc::SIGKILL);
                }
            }
        }
        let path = std::env::temp_dir().join(format!("rusty-mcp-escaped-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let script = "import os,sys,time\nif os.fork()==0:\n os.setsid()\n with open(sys.argv[1], 'w') as f: f.write(str(os.getpid()))\n time.sleep(60)\n os._exit(0)\nos._exit(0)";
        let cfg = ServerConfig {
            args: vec!["-c".into(), script.into(), path.display().to_string()],
            ..ServerConfig::default()
        };
        let server = Server::spawn("escaped", "python3", &cfg, Path::new(".")).unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        let escaped = loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break Escaped(pid);
                }
            }
            assert!(Instant::now() < end, "escaped fixture did not publish PID");
            std::thread::sleep(Duration::from_millis(5));
        };
        let stopped = server.reader_stop.clone();
        drop(server);
        assert_eq!(Arc::strong_count(&stopped), 1, "reader thread retained its cancellation state");
        assert_eq!(unsafe { libc::kill(escaped.0 as i32, 0) }, 0, "fixture must still hold stdout");
        drop(escaped);
        let _ = std::fs::remove_file(path);
    }
}
