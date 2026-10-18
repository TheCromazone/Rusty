//! Daytona's REST API, as used by the pinned Python SDK 0.220.0.
//!
//! Control plane (`DAYTONA_API_URL`, default `https://app.daytona.io/api`):
//! `/snapshots`, `/sandbox`, `/object-storage/push-access`. Each sandbox's
//! toolbox lives at `<toolboxProxyUrl>/<sandbox id>/...`: `/process/execute`,
//! `/process/session`, `/files/upload-v2` and `/files/download`. Both take
//! `Authorization: Bearer <DAYTONA_API_KEY>`.

use anyhow::{bail, Context, Result};
use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::{Remote, Sandboxes};

pub const DEFAULT_API_URL: &str = "https://app.daytona.io/api";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Credentials and endpoint, read once at startup.
#[derive(Clone)]
pub struct Config {
    pub api_url: String,
    api_key: String,
    pub target: Option<String>,
}

impl Config {
    /// `api_url` comes from the process environment captured before any .env
    /// file was loaded, as the SDK does: a project's .env must not be able to
    /// send your API key to another host.
    pub fn new(api_url: Option<String>, api_key: Option<String>, target: Option<String>) -> Result<Self> {
        let api_key = api_key.filter(|k| !k.is_empty()).context(
            "DAYTONA_API_KEY is not set. Put it in your environment or a private .env (never on the command line).",
        )?;
        let api_url = api_url.filter(|u| !u.is_empty()).unwrap_or_else(|| DEFAULT_API_URL.into());
        Ok(Self { api_url: api_url.trim_end_matches('/').into(), api_key, target: target.filter(|t| !t.is_empty()) })
    }
}

/// An API error. The message never includes headers, so never the key.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Daytona API returned {}: {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ApiError>().is_some_and(|e| e.status == 404)
}

fn client_for(url: &str) -> Result<Client> {
    let mut b =
        Client::builder().connect_timeout(CONNECT_TIMEOUT).user_agent(concat!("rusty/", env!("CARGO_PKG_VERSION")));
    // Loopback is never proxied (fixtures and local gateways).
    let loopback = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_owned)).is_some_and(|h| {
        h == "localhost" || h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok_and(|a| a.is_loopback())
    });
    if loopback {
        b = b.no_proxy();
    }
    Ok(b.build()?)
}

fn auth(rb: RequestBuilder, key: &str) -> RequestBuilder {
    rb.bearer_auth(key).header("X-Daytona-Source", "rusty")
}

/// Turns a non-2xx response into an `ApiError` with the server's message.
fn check(resp: Response) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().unwrap_or_default();
    let message = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| match &v["message"] {
            Value::String(s) => Some(s.clone()),
            Value::Array(a) => Some(a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")),
            _ => None,
        })
        .unwrap_or_else(|| text.chars().take(300).collect());
    Err(ApiError { status: status.as_u16(), message }.into())
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub id: String,
    pub name: String,
    pub state: String,
    pub error_reason: Option<String>,
}

impl Snapshot {
    fn from(v: &Value) -> Self {
        Self {
            id: v["id"].as_str().unwrap_or_default().into(),
            name: v["name"].as_str().unwrap_or_default().into(),
            state: v["state"].as_str().unwrap_or_default().into(),
            error_reason: v["errorReason"].as_str().map(Into::into),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Sandbox {
    pub id: String,
    pub state: String,
    pub labels: BTreeMap<String, String>,
    pub toolbox_proxy_url: Option<String>,
    pub error_reason: Option<String>,
}

impl Sandbox {
    fn from(v: &Value) -> Self {
        Self {
            id: v["id"].as_str().unwrap_or_default().into(),
            state: v["state"].as_str().unwrap_or_default().into(),
            labels: v["labels"]
                .as_object()
                .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
                .unwrap_or_default(),
            toolbox_proxy_url: v["toolboxProxyUrl"].as_str().filter(|s| !s.is_empty()).map(Into::into),
            error_reason: v["errorReason"].as_str().map(Into::into),
        }
    }
}

/// Temporary credentials for the build-context bucket.
#[derive(Clone, Debug)]
pub struct PushAccess {
    pub access_key: String,
    pub secret: String,
    pub session_token: String,
    pub storage_url: String,
    pub organization_id: String,
    pub bucket: String,
    pub region: String,
}

/// The control-plane client.
pub struct Daytona {
    http: Client,
    config: Config,
}

impl Daytona {
    pub fn new(config: Config) -> Result<Self> {
        Ok(Self { http: client_for(&config.api_url)?, config })
    }

    pub fn target(&self) -> Option<&str> {
        self.config.target.as_deref()
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        auth(self.http.request(method, format!("{}{path}", self.config.api_url)), &self.config.api_key)
    }

    fn call(&self, rb: RequestBuilder, timeout: Duration) -> Result<Value> {
        let resp = check(rb.timeout(timeout).send().context("Daytona API request failed")?)?;
        let text = resp.text()?;
        if text.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text).context("Daytona API returned invalid JSON")
    }

    fn get(&self, path: &str) -> Result<Value> {
        self.call(self.request(Method::GET, path), Duration::from_secs(60))
    }

    // ------------------------------------------------------------ snapshots

    /// `GET /snapshots/{id or name}`; None when it doesn't exist.
    pub fn snapshot(&self, name: &str) -> Result<Option<Snapshot>> {
        match self.get(&format!("/snapshots/{}", segment(name))) {
            Ok(v) => Ok(Some(Snapshot::from(&v))),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every page of `GET /snapshots`.
    pub fn snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut all = Vec::new();
        for page in 1.. {
            let v = self.get(&format!("/snapshots?page={page}&limit=100"))?;
            let items = v["items"].as_array().context("invalid snapshot list")?;
            all.extend(items.iter().map(Snapshot::from));
            let pages = v["totalPages"].as_u64().unwrap_or(1);
            if items.is_empty() || page >= pages {
                break;
            }
        }
        Ok(all)
    }

    /// `DELETE /snapshots/{id}`.
    pub fn delete_snapshot(&self, id: &str) -> Result<()> {
        self.call(self.request(Method::DELETE, &format!("/snapshots/{}", segment(id))), Duration::from_secs(60))?;
        Ok(())
    }

    /// `GET /object-storage/push-access`.
    pub fn push_access(&self) -> Result<PushAccess> {
        let v = self.get("/object-storage/push-access")?;
        let s = |k: &str| v[k].as_str().map(str::to_owned).with_context(|| format!("push access lacks {k}"));
        Ok(PushAccess {
            access_key: s("accessKey")?,
            secret: s("secret")?,
            session_token: s("sessionToken")?,
            storage_url: s("storageUrl")?,
            organization_id: s("organizationId")?,
            bucket: s("bucket")?,
            region: v["region"].as_str().unwrap_or("us-east-1").into(),
        })
    }

    /// `POST /snapshots` with a build, then polls once a second until it is
    /// active, reporting state changes and following the build log.
    pub fn create_snapshot(&self, body: Value, log: &dyn Fn(&str)) -> Result<Snapshot> {
        let mut snap =
            Snapshot::from(&self.call(self.request(Method::POST, "/snapshots").json(&body), Duration::from_secs(120))?);
        let terminal = |s: &str| matches!(s, "active" | "error" | "build_failed");
        log(&format!("Creating snapshot {} ({})", snap.name, snap.state));
        let mut previous = snap.state.clone();
        let mut streaming = false;
        while !terminal(&snap.state) {
            if !streaming && snap.state != "pending" {
                streaming = true;
                self.follow_build_log(&snap.id);
            }
            if previous != snap.state {
                log(&format!("Creating snapshot {} ({})", snap.name, snap.state));
                previous = snap.state.clone();
            }
            std::thread::sleep(Duration::from_secs(1));
            snap = Snapshot::from(&self.get(&format!("/snapshots/{}", segment(&snap.id)))?);
        }
        if snap.state != "active" {
            bail!(
                "Failed to create snapshot {}, reason: {}",
                snap.name,
                snap.error_reason.as_deref().unwrap_or("unknown")
            );
        }
        log(&format!("Created snapshot {} ({})", snap.name, snap.state));
        Ok(snap)
    }

    /// `GET /snapshots/{id}/build-logs-url`, then streams `<url>?follow=true`
    /// to stderr from a detached thread; polling decides when the build ends.
    fn follow_build_log(&self, id: &str) {
        let Ok(v) = self.get(&format!("/snapshots/{}/build-logs-url", segment(id))) else { return };
        let Some(url) = v["url"].as_str() else { return };
        let rb = auth(self.http.get(format!("{url}?follow=true")), &self.config.api_key)
            .timeout(Duration::from_secs(3 * 3600));
        std::thread::spawn(move || {
            use std::io::BufRead;
            let Ok(resp) = rb.send().and_then(Response::error_for_status) else { return };
            for line in std::io::BufReader::new(resp).lines() {
                match line {
                    Ok(l) => eprintln!("{}", l.trim_end()),
                    Err(_) => return,
                }
            }
        });
    }

    // ------------------------------------------------------------ sandboxes

    /// `GET /sandbox/{id or name}`.
    pub fn sandbox(&self, id: &str) -> Result<Sandbox> {
        if id.is_empty() {
            bail!("a sandbox id or name is required");
        }
        Ok(Sandbox::from(&self.get(&format!("/sandbox/{}", segment(id)))?))
    }

    /// `POST /sandbox` from a snapshot. `on_created` sees the id before the
    /// wait for `started`, so a failed start still leaves a record.
    pub fn create_sandbox(
        &self,
        snapshot: &str,
        env: &BTreeMap<String, String>,
        labels: &BTreeMap<String, String>,
        auto_stop_interval: u32,
        timeout: Duration,
        on_created: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<Sandbox> {
        let started = Instant::now();
        let mut body = json!({
            "snapshot": snapshot,
            "env": env,
            "labels": labels,
            "autoStopInterval": auto_stop_interval,
            "volumes": [],
        });
        if let Some(target) = &self.config.target {
            body["target"] = json!(target);
        }
        let sandbox = Sandbox::from(&self.call(self.request(Method::POST, "/sandbox").json(&body), timeout)?);
        on_created(&sandbox.id)?;
        self.wait_started(sandbox, started + timeout)
    }

    fn wait_started(&self, mut sandbox: Sandbox, deadline: Instant) -> Result<Sandbox> {
        loop {
            match sandbox.state.as_str() {
                "started" => return Ok(sandbox),
                "error" | "build_failed" | "destroyed" => bail!(
                    "sandbox {} failed to start ({}): {}",
                    sandbox.id,
                    sandbox.state,
                    sandbox.error_reason.as_deref().unwrap_or("no reason given")
                ),
                _ => {}
            }
            if Instant::now() > deadline {
                bail!("sandbox {} did not start in time (state {})", sandbox.id, sandbox.state);
            }
            std::thread::sleep(Duration::from_millis(500));
            sandbox = self.sandbox(&sandbox.id)?;
        }
    }

    /// The sandbox's toolbox, for commands and files.
    pub fn remote(&self, sandbox: &Sandbox) -> Result<Toolbox> {
        let base = match &sandbox.toolbox_proxy_url {
            Some(url) => url.clone(),
            None => self.get(&format!("/sandbox/{}/toolbox-proxy-url", segment(&sandbox.id)))?["url"]
                .as_str()
                .context("Daytona returned no toolbox URL")?
                .to_string(),
        };
        let base = format!("{}/{}", base.trim_end_matches('/'), segment(&sandbox.id));
        Ok(Toolbox {
            http: client_for(&base)?,
            base,
            key: self.config.api_key.clone(),
            id: sandbox.id.clone(),
            labels: sandbox.labels.clone(),
            home: OnceLock::new(),
        })
    }
}

impl Sandboxes for Daytona {
    /// `DELETE /sandbox/{id}`.
    fn delete_sandbox(&self, id: &str) -> Result<()> {
        self.call(self.request(Method::DELETE, &format!("/sandbox/{}", segment(id))), Duration::from_secs(60))?;
        Ok(())
    }
}

/// Percent-encodes one path segment.
pub fn segment(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One sandbox's toolbox API.
pub struct Toolbox {
    http: Client,
    base: String,
    key: String,
    id: String,
    labels: BTreeMap<String, String>,
    home: OnceLock<String>,
}

impl Toolbox {
    fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let resp = auth(self.http.post(format!("{}{path}", self.base)), &self.key)
            .json(body)
            .timeout(timeout)
            .send()
            .context("Daytona toolbox request failed")?;
        let text = check(resp)?.text()?;
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    /// `$HOME` in a sandbox path, resolved once.
    fn path(&self, p: &str) -> Result<String> {
        if !p.contains("$HOME") {
            return Ok(p.into());
        }
        let home = match self.home.get() {
            Some(h) => h.clone(),
            None => {
                let (_, out) = self.sh("printf %s $HOME", 30)?;
                let h = out.trim().to_string();
                if h.is_empty() {
                    bail!("could not resolve $HOME in the sandbox");
                }
                self.home.get_or_init(|| h).clone()
            }
        };
        Ok(p.replace("$HOME", &home))
    }

    fn file_url(&self, endpoint: &str, path: &str) -> Result<reqwest::Url> {
        Ok(reqwest::Url::parse_with_params(&format!("{}{endpoint}", self.base), [("path", path)])?)
    }
}

impl Remote for Toolbox {
    fn id(&self) -> &str {
        &self.id
    }

    fn labels(&self) -> BTreeMap<String, String> {
        self.labels.clone()
    }

    /// `POST /process/execute`. No `envs`: the host environment never travels.
    fn sh(&self, cmd: &str, timeout: u64) -> Result<(i32, String)> {
        let body = json!({"command": format!("bash -lc {}", super::shell::quote(cmd)), "timeout": timeout});
        let v = self.post("/process/execute", &body, Duration::from_secs(timeout + 5))?;
        let code = v["exitCode"].as_i64().or_else(|| v["code"].as_i64()).context("toolbox returned no exit code")?;
        Ok((code as i32, v["result"].as_str().unwrap_or_default().to_string()))
    }

    /// `POST /process/session`, then `POST /process/session/{id}/exec` with
    /// `runAsync`.
    fn start(&self, cmd: &str, session: &str) -> Result<String> {
        // Already there after a reconnect.
        let _ = self.post("/process/session", &json!({"sessionId": session}), Duration::from_secs(60));
        let body = json!({"command": format!("bash -lc {}", super::shell::quote(cmd)), "runAsync": true});
        let v = self.post(&format!("/process/session/{}/exec", segment(session)), &body, Duration::from_secs(60))?;
        Ok(v["cmdId"].as_str().context("toolbox returned no command id")?.to_string())
    }

    /// `POST /files/upload-v2?path=...` with the raw bytes.
    fn upload(&self, local: &Path, path: &str) -> Result<()> {
        let data = std::fs::File::open(local).with_context(|| format!("reading {}", local.display()))?;
        let url = self.file_url("/files/upload-v2", &self.path(path)?)?;
        let resp = auth(self.http.post(url), &self.key)
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .context("Daytona upload failed")?;
        check(resp)?;
        Ok(())
    }

    /// `GET /files/download?path=...`.
    fn download(&self, path: &str) -> Option<Vec<u8>> {
        let url = self.file_url("/files/download", &self.path(path).ok()?).ok()?;
        let resp = auth(self.http.get(url), &self.key).timeout(Duration::from_secs(30 * 60)).send().ok()?;
        let resp = check(resp).ok()?;
        resp.bytes().ok().map(|b| b.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_needs_a_key_and_defaults_the_url() {
        assert!(Config::new(None, None, None).is_err());
        assert!(Config::new(None, Some(String::new()), None).is_err());
        let c = Config::new(None, Some("k".into()), Some(String::new())).unwrap();
        assert_eq!(c.api_url, DEFAULT_API_URL);
        assert_eq!(c.target, None);
        let c = Config::new(Some("http://127.0.0.1:1/api/".into()), Some("k".into()), Some("eu".into())).unwrap();
        assert_eq!(c.api_url, "http://127.0.0.1:1/api");
        assert_eq!(c.target.as_deref(), Some("eu"));
    }

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(segment("rusty-0123abcd"), "rusty-0123abcd");
        assert_eq!(segment("a/b c?"), "a%2Fb%20c%3F");
    }
}
