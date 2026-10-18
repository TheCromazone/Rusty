//! Tool placement is independent of reasoning, memory and permissions.
//! The optional localhost bridge (`rusty-cloud hybrid`) owns the Daytona
//! connection. Rusty sends tool requests to it; neither model credentials nor
//! local files travel with them.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::{
    infra,
    permissions::{Policy, Verdict},
    tools,
};

const MAX_RPC: u64 = 1024 * 1024;
pub type ShellResult = (Option<Option<i32>>, String, String);

#[derive(Clone, Default)]
pub enum Backend {
    #[default]
    Local,
    Remote(std::sync::Arc<Bridge>),
}

pub struct Bridge {
    placement: &'static str,
    client: reqwest::blocking::Client,
    url: String,
    token: String,
    pub sandbox: String,
    pub cwd: PathBuf,
    description: Value,
}

impl Backend {
    pub fn connect(placement: &str) -> Result<Self> {
        match placement {
            "local" => Ok(Self::Local),
            "daytona" | "ssh" => {
                let missing = || {
                    if placement == "ssh" {
                        anyhow::anyhow!("SSH tools need the tool bridge. Run: rusty-cloud ssh --host HOST --workspace /path/to/project --local-binary PATH")
                    } else {
                        anyhow::anyhow!("Daytona tools need the tool bridge. Run: rusty-cloud hybrid --sandbox ID --workspace /home/daytona/work --local-binary PATH")
                    }
                };
                let raw = std::env::var("RUSTY_TOOL_BRIDGE_URL").map_err(|_| missing())?;
                let url = reqwest::Url::parse(&raw).context("invalid tool bridge URL")?;
                let loopback = url
                    .host_str()
                    .is_some_and(|h| h == "localhost" || h.parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback()));
                if url.scheme() != "http"
                    || !loopback
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                    || url.path() != "/"
                {
                    bail!("the Daytona tool bridge must be a localhost HTTP origin");
                }
                let token = std::env::var("RUSTY_TOOL_BRIDGE_TOKEN").map_err(|_| missing())?;
                if token.len() < 32 {
                    bail!("tool bridge token is too short");
                }
                let bridge = Bridge {
                    placement: if placement == "ssh" { "ssh" } else { "daytona" },
                    client: reqwest::blocking::Client::builder()
                        .no_proxy()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(Duration::from_secs(3))
                        .build()?,
                    url: raw.trim_end_matches('/').to_string(),
                    token,
                    sandbox: String::new(),
                    cwd: PathBuf::new(),
                    description: Value::Null,
                };
                let description = bridge.rpc(json!({"op":"describe"}), 30)?;
                if description["protocol"] != 1 {
                    bail!("snapshot lacks tool protocol v1; rebuild the Rusty snapshot");
                }
                let cwd = description["cwd"].as_str().context("bridge did not return its workspace")?;
                if !std::path::Path::new(cwd).is_absolute() {
                    bail!("bridge workspace must be absolute");
                }
                let sandbox = description["sandbox"].as_str().context("bridge did not return its sandbox")?;
                Ok(Self::Remote(std::sync::Arc::new(Bridge {
                    cwd: cwd.into(),
                    sandbox: sandbox.into(),
                    description: description.clone(),
                    ..bridge
                })))
            }
            _ => bail!("unknown tool placement `{placement}` (local, daytona, ssh)"),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote(b) => b.placement,
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Local => "local".into(),
            Self::Remote(b) => format!("{} · {} · {}", b.placement, b.sandbox, b.cwd.display()),
        }
    }

    pub fn cwd(&self, local: &std::path::Path) -> PathBuf {
        match self {
            Self::Local => local.into(),
            Self::Remote(b) => b.cwd.clone(),
        }
    }

    pub fn identity(&self, local: &std::path::Path) -> String {
        match self {
            Self::Local => local.display().to_string(),
            Self::Remote(b) => format!("{}:{}:{}", b.placement, b.sandbox, b.cwd.display()),
        }
    }

    pub fn describe(&self, identity: bool) -> Result<Value> {
        match self {
            Self::Local => bail!("local tools have no bridge"),
            Self::Remote(b) => b.rpc(json!({"op":"describe", "identity":identity}), 30),
        }
    }

    pub fn initial_description(&self) -> Value {
        match self {
            Self::Local => Value::Null,
            Self::Remote(b) => b.description.clone(),
        }
    }

    pub fn decide(&self, name: &str, args: &Value, policy: &Policy, cwd: &std::path::Path) -> Result<Verdict> {
        match self {
            Self::Local => Ok(policy.decide(name, args, cwd)),
            Self::Remote(b) => {
                Ok(serde_json::from_value(b.rpc(json!({"op":"decide","name":name,"args":args,"policy":policy}), 30)?)?)
            }
        }
    }

    pub fn execute(&self, name: &str, args: &Value, policy: &Policy, approval: &Verdict) -> Result<String> {
        match self {
            Self::Local => tools::execute(name, args),
            Self::Remote(b) => {
                let timeout =
                    if name == "bash" { args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 600) } else { 120 };
                let data = b.rpc(
                    json!({"op":"execute","name":name,"args":args,"policy":policy,"approval":approval}),
                    timeout + 20,
                )?;
                Ok(data.as_str().context("invalid remote tool result")?.to_string())
            }
        }
    }

    /// Harness probes use the same location as the model's tools.
    pub fn shell(&self, command: &str, timeout: Duration) -> Result<ShellResult> {
        match self {
            Self::Local => tools::run(command, timeout),
            Self::Remote(b) => Ok(serde_json::from_value(b.rpc(
                json!({"op":"shell","command":command,"timeout":timeout.as_secs().clamp(1,600)}),
                timeout.as_secs().clamp(1, 600) + 20,
            )?)?),
        }
    }

    pub fn snapshot(
        &self,
        harness: &infra::Harness,
        action: &infra::Action,
        command: &str,
    ) -> std::result::Result<PathBuf, String> {
        match self {
            Self::Local => harness.snapshot(action, command),
            Self::Remote(b) => {
                let local = harness.snapshot_with(action, command, |cmd, timeout| self.shell(cmd, timeout))?;
                let result = (|| -> Result<PathBuf> {
                    let mut files = serde_json::Map::new();
                    for entry in std::fs::read_dir(&local)? {
                        let path = entry?.path();
                        files.insert(
                            path.file_name().context("invalid snapshot name")?.to_string_lossy().into(),
                            json!(std::fs::read_to_string(&path)?),
                        );
                    }
                    let name = local.file_name().context("invalid snapshot directory")?.to_string_lossy();
                    let data = b.rpc(json!({"op":"snapshot","name":name,"files":files}), 30)?;
                    Ok(data.as_str().context("invalid remote snapshot path")?.into())
                })();
                result.map_err(|e| format!("snapshot kept at {}; remote staging failed: {e:#}", local.display()))
            }
        }
    }
}

impl Bridge {
    fn rpc(&self, request: Value, timeout: u64) -> Result<Value> {
        let bytes = serde_json::to_vec(&request)?;
        if bytes.len() as u64 > MAX_RPC {
            bail!("remote tool request exceeds 1 MiB");
        }
        let client = self.client.clone();
        let url = format!("{}/rpc", self.url);
        let token = self.token.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = (|| -> Result<Value> {
                let response = client
                    .post(url)
                    .bearer_auth(token)
                    .header("Content-Type", "application/json")
                    .body(bytes)
                    .timeout(Duration::from_secs(timeout))
                    .send()
                    .context(
                        "Daytona bridge request failed; remote execution may be incomplete, do not blindly retry",
                    )?;
                if !response.status().is_success() {
                    bail!("Daytona bridge returned HTTP {}; no local fallback", response.status());
                }
                let mut body = Vec::new();
                response.take(MAX_RPC + 1).read_to_end(&mut body)?;
                if body.len() as u64 > MAX_RPC {
                    bail!("remote tool response exceeds 1 MiB");
                }
                let value: Value = serde_json::from_slice(&body).context("invalid remote tool response")?;
                if value["ok"] != true {
                    bail!("{}", value["error"].as_str().unwrap_or("remote tool failed"));
                }
                Ok(value["data"].clone())
            })();
            let _ = tx.send(result);
        });
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("tool bridge connection closed"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    crate::signal::poll_keys();
                    if crate::signal::interrupted() {
                        bail!("interrupted: remote tool outcome is unknown; inspect the sandbox before retrying");
                    }
                }
            }
        }
    }
}

/// Private stdio endpoint the tool bridge invokes inside the sandbox. Never
/// loads dotenv, memory, model clients or UI; it uses the very same Rust tools.
pub fn serve_rpc() -> Result<i32> {
    let mut bytes = Vec::new();
    std::io::stdin().take(MAX_RPC + 1).read_to_end(&mut bytes)?;
    let result = (|| -> Result<Value> {
        if bytes.len() as u64 > MAX_RPC {
            bail!("tool request exceeds 1 MiB");
        }
        let v: Value = serde_json::from_slice(&bytes)?;
        let cwd = std::env::current_dir()?.canonicalize()?;
        match v["op"].as_str().unwrap_or("") {
            "describe" => {
                let target = if v["identity"] == true {
                    infra::Target::detect_with_identity(&cwd)
                } else {
                    infra::Target::detect(&cwd)
                };
                Ok(json!({"protocol":1,"cwd":cwd,"target":target,"notes":crate::agent::load_project_notes(&cwd)}))
            }
            "decide" | "execute" => {
                let policy: Policy = serde_json::from_value(v["policy"].clone())?;
                let name = v["name"].as_str().context("missing tool name")?;
                let verdict = policy.decide(name, &v["args"], &cwd);
                if v["op"] == "decide" {
                    return Ok(serde_json::to_value(verdict)?);
                }
                let approval: Verdict = serde_json::from_value(v["approval"].clone())?;
                if matches!(verdict, Verdict::Deny(_)) || verdict != approval {
                    bail!("remote permissions changed or denied this call; inspect it again before proceeding");
                }
                Ok(json!(tools::execute(name, &v["args"])?))
            }
            "shell" => Ok(json!(tools::run(
                v["command"].as_str().context("missing command")?,
                Duration::from_secs(v["timeout"].as_u64().unwrap_or(120).clamp(1, 600))
            )?)),
            "snapshot" => {
                let name = v["name"].as_str().context("missing snapshot name")?;
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    bail!("invalid snapshot directory");
                }
                // Keep operational snapshots outside the checkout so ordinary
                // git add/export cannot include them by accident.
                let dir = crate::config::config_dir()
                    .context("no private sandbox config directory")?
                    .join("snapshots")
                    .join(name);
                let files = v["files"].as_object().context("missing snapshot files")?;
                // Only the fixed names made by the harness may be staged.
                for (name, text) in files {
                    if !matches!(
                        name.as_str(),
                        "before.yaml"
                            | "rollback.yaml"
                            | "command.txt"
                            | "terraform.tfstate"
                            | "values.yaml"
                            | "manifest.yaml"
                    ) {
                        bail!("invalid snapshot file");
                    }
                    let text = text.as_str().context("invalid snapshot contents")?;
                    if infra::redact(text).1 > 0 {
                        bail!("snapshot contains credentials");
                    }
                    let path = dir.join(name);
                    if let Some(parent) = path.parent() {
                        rusty::privacy::private_dir(parent)?;
                    }
                    let mut file = rusty::privacy::private_file(&path, false)?;
                    file.write_all(text.as_bytes())?;
                }
                Ok(json!(dir))
            }
            _ => bail!("unknown tool protocol operation"),
        }
    })();
    let response = match result {
        Ok(data) => json!({"ok":true,"data":data}),
        Err(e) => json!({"ok":false,"error":infra::redact(&format!("{e:#}")).0}),
    };
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &response)?;
    writeln!(out)?;
    Ok(0)
}
