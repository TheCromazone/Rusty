//! A minimal MCP client: stdio servers from `.mcp.json` (or `RUSTY_MCP_CONFIG`),
//! their tools offered to the model as `mcp__<server>__<tool>`.
//!
//! Newline-delimited JSON-RPC 2.0 over the server's stdin/stdout, per the MCP
//! stdio transport. Only tools are supported; resources and prompts are not.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::RwLock;
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
    lines: Receiver<String>,
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
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting `{command}`"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        // A reader thread, so a silent server can never block past a deadline.
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { name: name.to_string(), child, stdin, lines, next_id: 1, tools: Vec::new(), ignored: Vec::new() })
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

    fn send(&mut self, message: &Value) -> Result<()> {
        writeln!(self.stdin, "{message}").and_then(|()| self.stdin.flush()).context("the MCP server closed its input")
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match self.lines.recv_timeout(left) {
                Ok(line) => line,
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

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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

/// Manage the existing MCP manifest without starting anything on add/list.
/// Preserve unknown fields and existing env values; never echo credentials.
pub fn manage(cwd: &Path, policy: &crate::permissions::Policy, args: &[String]) -> Result<String> {
    use std::io::Write;
    let path =
        std::env::var_os("RUSTY_MCP_CONFIG").map(std::path::PathBuf::from).unwrap_or_else(|| cwd.join(".mcp.json"));
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    let action = args.first().map(String::as_str).unwrap_or("list");
    if action == "help" {
        return Ok("MCP: /integrations mcp list | add NAME COMMAND [ARGS...] | remove NAME | check. Add saves a required stdio server; it starts only on check or the next session. Use the CLI form for arguments containing spaces. Existing .mcp.json / RUSTY_MCP_CONFIG remains the source of truth. Configure authentication in the server's own native flow or environment; do not put tokens in command arguments. Direct HTTP transport is not supported; an explicitly configured stdio bridge can supply it.".into());
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
            anyhow::ensure!(args.len() >= 3, "usage: /integrations mcp add NAME COMMAND [ARGS...]");
            anyhow::ensure!(
                !servers.contains_key(name),
                "MCP server already exists; remove it explicitly before replacing it"
            );
            let start = if args.get(2).is_some_and(|a| a == "--") { 3 } else { 2 };
            let command = args.get(start).context("MCP server command required")?;
            anyhow::ensure!(!command.trim().is_empty(), "MCP server command required");
            servers.insert(name.clone(), serde_json::json!({"command":command,"args":args[start+1..],"required":true}));
        }
        _ => {
            anyhow::ensure!(args.len() == 2, "usage: /integrations mcp remove NAME");
            anyhow::ensure!(servers.remove(name).is_some(), "MCP server not configured");
        }
    }
    if path.exists() {
        anyhow::ensure!(
            std::fs::symlink_metadata(&path)?.file_type().is_file(),
            "MCP config must be a regular file, not a symlink"
        );
    }
    let parent = path.parent().context("MCP config directory missing")?;
    std::fs::create_dir_all(parent)?;
    let pending = path.with_extension("json.pending");
    let mut file = rusty::privacy::private_file(&pending, false)?;
    // Unlike transcript persistence, a config edit must not scrub an existing
    // server's secret value into a nonfunctional placeholder.
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&pending, &path)?;
    Ok(format!("MCP server {name} {action} saved; existing sessions keep their current connections"))
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
        Server { name: name.into(), child, stdin, lines: mpsc::channel().1, next_id: 1, tools, ignored: Vec::new() }
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
}
