//! A minimal MCP client: stdio servers from `.mcp.json` (or `RUSTY_MCP_CONFIG`),
//! their tools offered to the model as `mcp__<server>__<tool>`.
//!
//! Newline-delimited JSON-RPC 2.0 over the server's stdin/stdout, per the MCP
//! stdio transport. Only tools are supported; resources and prompts are not.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
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

#[derive(Deserialize)]
struct Config {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, Value>,
}

#[derive(Deserialize, Default)]
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
    stdin: ChildStdin,
    lines: Receiver<Result<String, String>>,
    reader_stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    next_id: u64,
    tools: Vec<Tool>,
    /// Features the server offers that rusty doesn't use.
    ignored: Vec<String>,
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
        let server = self
            .servers
            .iter_mut()
            .find(|s| s.tools.iter().any(|t| t.exposed == exposed))
            .ok_or_else(|| anyhow!("unknown MCP tool `{exposed}`"))?;
        let name = server.tools.iter().find(|t| t.exposed == exposed).map(|t| t.name.clone()).unwrap_or_default();
        let args = if args.is_object() { args.clone() } else { json!({}) };
        let result = server.request("tools/call", json!({"name": name, "arguments": args}), call_timeout())?;
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

impl Server {
    /// Starts, initializes and lists one server; an error says which stage failed.
    fn start(name: &str, command: &str, cfg: &ServerConfig, cwd: &Path) -> Result<Self, (&'static str, Error)> {
        let mut server = Self::spawn(name, command, cfg, cwd).map_err(|e| ("start_failed", e))?;
        server.initialize().map_err(|e| ("handshake_failed", e))?;
        server.discover().map_err(|e| ("discovery_failed", e))?;
        Ok(server)
    }

    fn spawn(name: &str, command: &str, cfg: &ServerConfig, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(command)
            .args(&cfg.args)
            .envs(&cfg.env)
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
            stdin,
            lines,
            reader_stop: reader_stop.clone(),
            reader: None,
            next_id: 1,
            tools: Vec::new(),
            ignored: Vec::new(),
        };
        for fd in [server.stdin.as_raw_fd(), stdout.as_raw_fd()] {
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

    fn initialize(&mut self) -> Result<()> {
        let init = self.request(
            "initialize",
            json!({"protocolVersion": PROTOCOL, "capabilities": {},
                   "clientInfo": {"name": "rusty", "version": env!("CARGO_PKG_VERSION")}}),
            HANDSHAKE,
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
        self.notify("notifications/initialized")
    }

    fn discover(&mut self) -> Result<()> {
        let name = self.name.clone();
        // `tools/list` is paginated: follow `nextCursor` until it stops,
        // within bounds a looping or endless server can't get past.
        let (mut cursor, mut seen) = (None::<String>, HashSet::new());
        for page in 1.. {
            let params = cursor.as_ref().map_or(json!({}), |c| json!({ "cursor": c }));
            let listed = self.request("tools/list", params, HANDSHAKE)?;
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

    fn send(&mut self, message: &Value, deadline: Instant) -> Result<()> {
        let bytes = encode_frame(message)?;
        let mut rest = bytes.as_slice();
        while !rest.is_empty() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("MCP input timed out");
            }
            match self.stdin.write(rest) {
                Ok(0) => bail!("the MCP server closed its input"),
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: self.stdin.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                    let status =
                        unsafe { libc::poll(&mut fd, 1, left.as_millis().max(1).min(i32::MAX as u128) as i32) };
                    if status < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        return Err(std::io::Error::last_os_error()).context("waiting for MCP input");
                    }
                }
                Err(e) => return Err(e).context("the MCP server closed its input"),
            }
        }
        Ok(())
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}), Instant::now() + HANDSHAKE)
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let deadline = Instant::now() + timeout;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}), deadline)?;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("{method} timed out after {}s", timeout.as_secs());
            }
            let line = match self.lines.recv_timeout(left) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => bail!("MCP output rejected: {error}"),
                Err(RecvTimeoutError::Timeout) => bail!("{method} timed out after {}s", timeout.as_secs()),
                Err(RecvTimeoutError::Disconnected) => bail!("the MCP server `{}` exited", self.name),
            };
            // Skip notifications, server requests and stray output.
            let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
            if msg["id"].as_u64() != Some(id) || msg.get("method").is_some() {
                continue;
            }
            if let Some(err) = msg.get("error") {
                bail!("{method}: {}", err["message"].as_str().unwrap_or("error"));
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

impl Drop for Server {
    fn drop(&mut self) {
        self.reader_stop.store(true, Ordering::Release);
        // A launcher may exit before its server. Reap the child and stop its
        // owned process group so descendants cannot retain stdout/readers.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
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

#[cfg(test)]
mod tests {
    use super::*;

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
            stdin,
            lines: mpsc::channel().1,
            reader_stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            next_id: 1,
            tools,
            ignored: Vec::new(),
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
