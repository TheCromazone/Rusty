//! Codex sign-in: the ChatGPT account the Codex CLI logged in with, used
//! against the Codex backend's Responses API.
//!
//! `codex login` leaves tokens in `~/.codex/auth.json` (`CODEX_HOME`
//! overrides the folder). rusty reads them, refreshes the access token the
//! same way Codex does when it is about to expire or is refused, and writes
//! the refreshed tokens back so both tools stay signed in. No token is ever
//! printed or passed as an argument.
//!
//! The backend speaks the Responses API, not chat completions, so rusty's
//! OpenAI-shaped history is converted on the way out and the streamed items
//! are folded back into a `Reply`. Reasoning items come back encrypted and
//! are kept on the assistant message under `_codex` so the next request can
//! hand them back unchanged, the way Claude's thinking blocks are kept.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Response;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::execution::ExecutionMode;
use crate::llm::{Delta, Reply, ToolCall, Usage};

pub const BASE_URL: &str = "https://chatgpt.com/backend-api";
/// Used when `~/.codex/config.toml` names no model and no catalog is saved.
pub const DEFAULT_MODEL: &str = "gpt-5.3-codex";
/// Key under which an assistant message keeps the backend's reasoning items.
pub const ITEMS_KEY: &str = "_codex";
/// The OAuth client the Codex CLI signs in with; refreshes must use the same one.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REFRESH_URL: &str = "https://auth.openai.com/oauth/token";
/// Refresh this close to the access token's expiry, as Codex does.
const REFRESH_WINDOW_SECS: u64 = 5 * 60;
/// Never let a stalled OAuth refresh hold a request indefinitely. A root
/// budget can shorten this further at the call site.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

/// The Codex home folder: `CODEX_HOME`, else `~/.codex`.
pub fn home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex"))
}

/// The model Codex itself is configured to use (`model = "…"` in config.toml).
pub fn configured_model() -> Option<String> {
    model_in(&std::fs::read_to_string(home()?.join("config.toml")).ok()?)
}

fn model_in(text: &str) -> Option<String> {
    // Top-level keys only: a `model` under `[profiles.x]` is not the default.
    text.lines().map(str::trim).take_while(|l| !l.starts_with('[')).filter(|l| !l.starts_with('#')).find_map(|l| {
        let (k, v) = l.split_once('=')?;
        // A quoted value ends at its closing quote; a bare one at a comment.
        let v = v.trim();
        let v = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
            _ => v.split('#').next().unwrap_or("").trim(),
        };
        (k.trim() == "model" && !v.is_empty()).then(|| v.to_string())
    })
}

/// The models the backend offers this account, from the catalog Codex saves
/// as `models_cache.json` (the `list`-visible ones in Codex's own order).
/// Empty when Codex has not been run here yet.
pub fn listed_models() -> Vec<String> {
    let Some(text) = home().and_then(|h| std::fs::read_to_string(h.join("models_cache.json")).ok()) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    let mut models: Vec<(i64, String)> = v["models"]
        .as_array()
        .map(|ms| {
            ms.iter()
                .filter(|m| m["visibility"].as_str() == Some("list"))
                .filter_map(|m| Some((m["priority"].as_i64().unwrap_or(i64::MAX), m["slug"].as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    models.sort();
    models.into_iter().map(|(_, slug)| slug).collect()
}

/// The model to use: the one Codex is configured with, else the first the
/// backend lists for this account, else a name that used to be safe.
pub fn default_model() -> String {
    configured_model().or_else(|| listed_models().into_iter().next()).unwrap_or_else(|| DEFAULT_MODEL.into())
}

/// A fresh id in UUID form for this process's requests, the shape Codex
/// sends as its conversation id.
pub fn session_id() -> String {
    let r = |n: usize| crate::ui::rand(1 << n);
    format!("{:08x}-{:04x}-4{:03x}-{:04x}-{:06x}{:06x}", r(32), r(16), r(12), 0x8000 | r(14), r(24), r(24))
}

/// The signed-in ChatGPT account's tokens.
#[derive(Clone)]
pub struct Auth {
    path: PathBuf,
    access_token: String,
    refresh_token: String,
    account_id: String,
    /// The account's email from the id token, for the doctor; never a secret.
    pub email: Option<String>,
}

impl std::fmt::Debug for Auth {
    /// Never the tokens: an `{:?}` on an endpoint must not print a sign-in.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth").field("account_id", &self.account_id).field("email", &self.email).finish_non_exhaustive()
    }
}

impl Auth {
    /// The tokens Codex saved, when it is signed in with a ChatGPT account.
    pub fn load() -> Option<Auth> {
        Self::load_from(home()?.join("auth.json"))
    }

    pub fn load_from(path: PathBuf) -> Option<Auth> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()?;
        let t = &v["tokens"];
        let access_token = t["access_token"].as_str()?.to_string();
        let refresh_token = t["refresh_token"].as_str()?.to_string();
        if access_token.is_empty() || refresh_token.is_empty() {
            return None;
        }
        // The account id sits next to the tokens; older files only have it in the id token.
        let account_id = t["account_id"]
            .as_str()
            .map(String::from)
            .or_else(|| {
                jwt_claims(t["id_token"].as_str()?)?["https://api.openai.com/auth"]["chatgpt_account_id"]
                    .as_str()
                    .map(String::from)
            })
            .unwrap_or_default();
        let email = t["id_token"]
            .as_str()
            .and_then(jwt_claims)
            .and_then(|c| c["email"].as_str().or_else(|| c["profile"]["email"].as_str()).map(String::from));
        Some(Auth { path, access_token, refresh_token, account_id, email })
    }

    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    /// The access token expires within the refresh window (or carries no
    /// readable expiry, which is treated as fresh).
    pub fn expiring(&self) -> bool {
        jwt_claims(&self.access_token)
            .and_then(|c| c["exp"].as_u64())
            .is_some_and(|exp| exp.saturating_sub(rusty::unix_now()) < REFRESH_WINDOW_SECS)
    }

    /// Trades the refresh token for new tokens and saves them where Codex
    /// keeps them. A refresh token the auth service no longer accepts means
    /// signing in again; the error says so.
    pub fn refresh(&mut self, http: &reqwest::blocking::Client, timeout: Duration) -> Result<()> {
        let url = std::env::var("RUSTY_CODEX_REFRESH_URL").ok().filter(|u| !u.is_empty());
        self.refresh_at(http, timeout, url.as_deref().unwrap_or(REFRESH_URL))
    }

    fn refresh_at(&mut self, http: &reqwest::blocking::Client, timeout: Duration, url: &str) -> Result<()> {
        // Codex (or another rusty) may have refreshed already. Refresh tokens
        // are single use, so spending ours again could sign both tools out.
        if let Some(disk) = Self::load_from(self.path.clone()) {
            if disk.access_token != self.access_token && !disk.expiring() {
                *self = disk;
                return Ok(());
            }
            self.refresh_token = disk.refresh_token;
        }
        let deadline = std::time::Instant::now() + timeout;
        let body = json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": self.refresh_token});
        let resp = http.post(url).json(&body).timeout(timeout).send().context("codex token refresh failed")?;
        let status = resp.status();
        let text = resp.text().context("reading codex token refresh response")?;
        if !status.is_success() {
            // Another process may have spent the same refresh token a moment
            // earlier; its new tokens land on disk shortly after.
            for _ in 0..6 {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                if let Some(disk) = Self::load_from(self.path.clone()) {
                    if disk.refresh_token != self.refresh_token && !disk.expiring() {
                        *self = disk;
                        return Ok(());
                    }
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                std::thread::sleep(remaining.min(Duration::from_millis(250)));
            }
            let reason = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().or_else(|| v["error"]["code"].as_str()).map(String::from))
                .unwrap_or_default();
            bail!(
                "codex sign-in expired (HTTP {status}{}); run `codex login` and try again",
                if reason.is_empty() { String::new() } else { format!(", {reason}") }
            );
        }
        let v: Value = serde_json::from_str(&text).context("bad token refresh response")?;
        let access = v["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("refresh gave no access token"))?;
        self.access_token = access.to_string();
        if let Some(r) = v["refresh_token"].as_str().filter(|s| !s.is_empty()) {
            self.refresh_token = r.to_string();
        }
        self.save(v["id_token"].as_str())
    }

    /// Writes the current tokens back into auth.json, keeping everything
    /// else in the file (Codex's own fields) as it was.
    fn save(&self, id_token: Option<&str>) -> Result<()> {
        let mut v: Value = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({"auth_mode": "chatgpt"}));
        let t = v["tokens"].as_object_mut().map(std::mem::take).unwrap_or_default();
        let mut t = t;
        t.insert("access_token".into(), json!(self.access_token));
        t.insert("refresh_token".into(), json!(self.refresh_token));
        if let Some(id) = id_token {
            t.insert("id_token".into(), json!(id));
        }
        if !self.account_id.is_empty() {
            t.entry("account_id").or_insert(json!(self.account_id));
        }
        v["tokens"] = Value::Object(t);
        v["last_refresh"] = json!(rfc3339_now());
        // Private like the file Codex writes, and unique so two processes never share it.
        let tmp = self.path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::io::Write::write_all(
            &mut rusty::privacy::private_file(&tmp, false)?,
            serde_json::to_string_pretty(&v)?.as_bytes(),
        )?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// The payload of a JWT, without checking its signature: enough to read an
/// expiry or an email that the auth service itself issued.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url(payload)?).ok()
}

fn base64url(s: &str) -> Option<Vec<u8>> {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'+' => 62,
            b'/' => 63,
            c => T.iter().position(|t| *t == c)? as u32,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Now as `2026-10-17T18:04:05Z`, the shape Codex writes to `last_refresh`.
fn rfc3339_now() -> String {
    let secs = rusty::unix_now() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A Responses API request for the Codex backend from rusty's chat-shaped
/// history and tool list.
pub fn request(model: &str, messages: &[Value], tools: &Value, mode: ExecutionMode) -> Value {
    let (instructions, input) = convert(messages);
    let effort = match mode {
        ExecutionMode::Careful => "high",
        ExecutionMode::Standard => "medium",
        ExecutionMode::Vibe => "low",
    };
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": effort, "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": mode.max_tokens(),
    });
    let tools: Vec<Value> = tools
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| {
            let f = if t["function"].is_object() { &t["function"] } else { t };
            json!({
                "type": "function",
                "name": f["name"],
                "description": f["description"],
                "parameters": f["parameters"],
                "strict": false,
            })
        })
        .collect();
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

/// Splits chat-shaped messages into the request's instructions (every
/// system message) and its input items.
pub fn convert(messages: &[Value]) -> (String, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("user");
        let text = text_of(&m["content"]);
        match role {
            "system" => instructions.push(text),
            "assistant" => {
                for item in m[ITEMS_KEY].as_array().into_iter().flatten() {
                    input.push(item.clone());
                }
                if !text.is_empty() {
                    input.push(json!({"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": text}]}));
                }
                for (i, c) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
                    let id = c["id"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("call_{i}"));
                    let args = match &c["function"]["arguments"] {
                        Value::String(s) => s.clone(),
                        Value::Null => "{}".into(),
                        other => other.to_string(),
                    };
                    input.push(json!({"type": "function_call", "call_id": id,
                        "name": c["function"]["name"], "arguments": args}));
                }
            }
            "tool" => input.push(json!({"type": "function_call_output",
                "call_id": m["tool_call_id"].as_str().unwrap_or(""), "output": text})),
            _ => input.push(json!({"type": "message", "role": "user",
                "content": [{"type": "input_text", "text": text}]})),
        }
    }
    (instructions.join("\n\n"), input)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            parts.iter().filter_map(|p| p["text"].as_str().or_else(|| p.as_str())).collect::<Vec<_>>().join("")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A text or reasoning delta read in place (see `crate::sse`); other events
/// are rare enough to read as a `Value`.
#[derive(Deserialize, Default)]
#[serde(default)]
struct TextEvent<'a> {
    #[serde(rename = "type", borrow)]
    kind: crate::sse::Str<'a>,
    #[serde(borrow)]
    delta: crate::sse::Str<'a>,
}

/// Folds the backend's event stream into a `Reply`, calling `on` for every
/// text and reasoning-summary delta; `on` returning false stops early.
pub fn parse_stream(resp: Response, on: &mut dyn FnMut(Delta) -> bool) -> Result<Reply> {
    let mut reply = Reply { raw_key: ITEMS_KEY, ..Default::default() };
    let mut items: Vec<Value> = Vec::new();
    let mut streamed_text = false;
    let mut saw_terminal = false;
    let mut saw_data = false;
    let mut raw = String::new();

    let mut lines = crate::llm::lines(resp);
    while let Some(line) = lines.next_line() {
        let line = line.context("stream interrupted")?;
        let Some(data) = line.strip_prefix("data:") else {
            if !saw_data && !line.starts_with("event:") {
                raw.push_str(line);
            }
            continue;
        };
        saw_data = true;
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let mut spare = None;
        let Some(ev) = crate::sse::event::<TextEvent>(data, &mut spare) else { continue };
        match ev.kind.get().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(s) = ev.delta.get().filter(|s| !s.is_empty()) {
                    streamed_text = true;
                    reply.content.push_str(s);
                    if !on(Delta::Content(s)) {
                        reply.finish_reason = Some("interrupted".into());
                        return Ok(finish(reply, items));
                    }
                }
                continue;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(s) = ev.delta.get().filter(|s| !s.is_empty()) {
                    if !on(Delta::Reasoning(s)) {
                        reply.finish_reason = Some("interrupted".into());
                        return Ok(finish(reply, items));
                    }
                }
                continue;
            }
            _ => {}
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        match v["type"].as_str().unwrap_or("") {
            "error" => bail!("API error: {}", v["error"].as_str().map(String::from).unwrap_or_else(|| v.to_string())),
            "response.failed" => {
                // The code (rate_limit_exceeded, server_is_overloaded) is what marks it worth retrying.
                let e = &v["response"]["error"];
                let msg = e["message"].as_str().map(String::from).unwrap_or_else(|| e.to_string());
                match e["code"].as_str() {
                    Some(code) => bail!("API error: {code}: {msg}"),
                    None => bail!("API error: {msg}"),
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str().unwrap_or("") {
                    "function_call" => reply.tool_calls.push(ToolCall {
                        id: item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or("").to_string(),
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        arguments: item["arguments"]
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| item["arguments"].to_string()),
                    }),
                    "reasoning" => items.push(reasoning_item(item)),
                    // A server that sends no deltas still gives the whole message here.
                    "message" if !streamed_text => {
                        let text = text_of(&item["content"]);
                        if !text.is_empty() {
                            reply.content.push_str(&text);
                            if !on(Delta::Content(&text)) {
                                reply.finish_reason = Some("interrupted".into());
                                return Ok(finish(reply, items));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                saw_terminal = true;
                let r = &v["response"];
                if let Some(u) = r.get("usage").filter(|u| !u.is_null()) {
                    reply.usage = Some(Usage {
                        prompt: u["input_tokens"].as_u64().unwrap_or(0),
                        completion: u["output_tokens"].as_u64().unwrap_or(0),
                        cache_read: u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
                        ..Default::default()
                    });
                }
                let reason = r["incomplete_details"]["reason"].as_str().unwrap_or("");
                reply.finish_reason = Some(match reason {
                    "max_output_tokens" | "max_tokens" => "length".into(),
                    "content_filter" => "content_filter".into(),
                    _ if !reply.tool_calls.is_empty() => "tool_calls".into(),
                    _ => "stop".into(),
                });
            }
            _ => {}
        }
    }
    if !saw_data && !raw.trim().is_empty() {
        bail!("unexpected response: {}", crate::ui::truncate(raw.trim(), 300));
    }
    if !saw_terminal {
        bail!("stream interrupted: Codex Responses stream ended before a terminal event");
    }
    for (i, c) in reply.tool_calls.iter_mut().enumerate() {
        if c.id.is_empty() {
            c.id = format!("call_{i}");
        }
    }
    Ok(finish(reply, items))
}

fn finish(mut reply: Reply, items: Vec<Value>) -> Reply {
    if !items.is_empty() {
        reply.raw = Some(Value::Array(items));
    }
    reply
}

/// A reasoning item as the next request must hand it back: its id, the
/// encrypted content and the summary, nothing the API would reject.
fn reasoning_item(item: &Value) -> Value {
    let mut out = Map::new();
    out.insert("type".into(), json!("reasoning"));
    for key in ["id", "encrypted_content", "summary"] {
        if !item[key].is_null() {
            out.insert(key.into(), item[key].clone());
        }
    }
    if !out.contains_key("summary") {
        out.insert("summary".into(), json!([]));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_temp_dir() -> PathBuf {
        use ring::rand::{SecureRandom, SystemRandom};

        for _ in 0..64 {
            let mut bytes = [0; 16];
            SystemRandom::new().fill(&mut bytes).unwrap();
            let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("rusty-codex-{}-{suffix}", std::process::id()));
            match std::fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create auth fixture directory {}: {error}", dir.display()),
            }
        }
        panic!("could not allocate a unique auth fixture directory")
    }

    fn jwt(claims: Value) -> String {
        let b64 = |b: &[u8]| {
            const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut s = String::new();
            for chunk in b.chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
                for i in 0..chunk.len() + 1 {
                    s.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            s
        };
        format!("{}.{}.sig", b64(b"{\"alg\":\"none\"}"), b64(claims.to_string().as_bytes()))
    }

    #[test]
    fn reads_tokens_and_expiry_from_auth_json() {
        let dir = auth_temp_dir();
        let path = dir.join("auth.json");
        let fresh = jwt(json!({"exp": rusty::unix_now() + 3600}));
        let id =
            jwt(json!({"email": "dev@example.com", "https://api.openai.com/auth": {"chatgpt_account_id": "acct_1"}}));
        std::fs::write(
            &path,
            json!({"auth_mode": "chatgpt", "tokens": {"access_token": fresh, "refresh_token": "r1", "id_token": id}, "last_refresh": "x"})
                .to_string(),
        )
        .unwrap();
        let auth = Auth::load_from(path.clone()).unwrap();
        assert_eq!(auth.account_id(), "acct_1", "falls back to the id token's account");
        assert_eq!(auth.email.as_deref(), Some("dev@example.com"));
        assert!(!auth.expiring());

        let stale = jwt(json!({"exp": rusty::unix_now() + 60}));
        std::fs::write(
            &path,
            json!({"tokens": {"access_token": stale, "refresh_token": "r1", "account_id": "acct_2"}}).to_string(),
        )
        .unwrap();
        let auth = Auth::load_from(path.clone()).unwrap();
        assert_eq!(auth.account_id(), "acct_2");
        assert!(auth.expiring());
        assert!(Auth::load_from(dir.join("missing.json")).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn saving_keeps_codex_fields_and_updates_tokens() {
        let dir = auth_temp_dir();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "tokens": {"access_token": "a0", "refresh_token": "r0", "id_token": "i0", "account_id": "acct"}})
                .to_string(),
        )
        .unwrap();
        let mut auth = Auth::load_from(path.clone()).unwrap();
        auth.access_token = "a1".into();
        auth.refresh_token = "r1".into();
        auth.save(None).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["tokens"]["access_token"], "a1");
        assert_eq!(v["tokens"]["refresh_token"], "r1");
        assert_eq!(v["tokens"]["id_token"], "i0", "an untouched field stays");
        assert_eq!(v["auth_mode"], "chatgpt");
        assert!(v["last_refresh"].as_str().unwrap().ends_with('Z'));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp file left behind");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "tokens stay private");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn refresh_takes_tokens_another_process_already_refreshed() {
        let dir = auth_temp_dir();
        let path = dir.join("auth.json");
        let write = |access: &str, refresh: &str| {
            std::fs::write(
                &path,
                json!({"tokens": {"access_token": access, "refresh_token": refresh, "account_id": "a"}}).to_string(),
            )
            .unwrap()
        };
        write(&jwt(json!({"exp": rusty::unix_now() + 60})), "r0");
        let mut auth = Auth::load_from(path.clone()).unwrap();
        let newer = jwt(json!({"exp": rusty::unix_now() + 3600}));
        write(&newer, "r1");
        // No refresh server is reachable here: the newer tokens on disk are used without one.
        auth.refresh(&reqwest::blocking::Client::new(), REFRESH_TIMEOUT).unwrap();
        assert_eq!((auth.access_token(), auth.refresh_token.as_str()), (newer.as_str(), "r1"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stalled_refresh_is_bounded_by_its_request_timeout() {
        use std::io::Write;
        use std::net::TcpListener;

        let dir = auth_temp_dir();
        let path = dir.join("auth.json");
        std::fs::write(
            &path,
            json!({"tokens": {"access_token": jwt(json!({"exp": rusty::unix_now() + 60})), "refresh_token": "offline-r0", "account_id": "a"}})
                .to_string(),
        )
        .unwrap();
        let mut auth = Auth::load_from(path).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Simulate an OAuth server that accepts the refresh but never
            // completes its body. The caller must not wait for the blocking
            // client's former 30-second default (or indefinitely).
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });

        let http = reqwest::blocking::Client::builder().timeout(None).build().unwrap();
        let started = std::time::Instant::now();
        let result = auth.refresh_at(&http, Duration::from_millis(100), &url);
        let elapsed = started.elapsed();
        server.join().unwrap();
        assert!(result.is_err(), "a refresh with no response must fail");
        assert!(elapsed < Duration::from_millis(400), "refresh exceeded its 100ms cap: {elapsed:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn history_becomes_instructions_and_items() {
        let messages = vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "list files"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}}],
                "_codex": [{"type": "reasoning", "id": "rs_1", "encrypted_content": "zzz", "summary": []}], "_anthropic": [{"type": "text"}]}),
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a.rs"}),
            json!({"role": "assistant", "content": "one file"}),
        ];
        let body = request(
            "gpt-x",
            &messages,
            &json!([crate::tools::tool("bash", "run", json!({}), &[])]),
            ExecutionMode::Standard,
        );
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert!(body["tools"][0]["function"].is_null(), "flattened for the Responses API");
        let input = body["input"].as_array().unwrap();
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap()).collect();
        assert_eq!(types, ["message", "reasoning", "function_call", "function_call_output", "message"]);
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[1]["encrypted_content"], "zzz");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "a.rs");
        assert_eq!(input[4]["content"][0]["type"], "output_text");
        assert!(!body.to_string().contains("_anthropic"), "another provider's private blocks never leave");
    }

    fn stream(events: &[Value]) -> Response {
        let body: String =
            events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap_or(""), e)).collect();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                .unwrap();
        });
        reqwest::blocking::get(format!("http://{addr}/")).unwrap()
    }

    #[test]
    fn stream_folds_text_calls_reasoning_and_usage() {
        let resp = stream(&[
            json!({"type": "response.created", "response": {}}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
            json!({"type": "response.output_item.done", "item": {"type": "reasoning", "id": "rs_1", "encrypted_content": "enc", "summary": [{"type": "summary_text", "text": "thinking"}], "status": "completed"}}),
            json!({"type": "response.output_text.delta", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "delta": "lo"}),
            json!({"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]}}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "call_9", "name": "read_file", "arguments": "{\"path\":\"a\"}"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 120, "output_tokens": 7, "input_tokens_details": {"cached_tokens": 100}}}}),
        ]);
        let mut seen = Vec::new();
        let reply = parse_stream(resp, &mut |d| {
            seen.push(match d {
                Delta::Content(s) => format!("c:{s}"),
                Delta::Reasoning(s) => format!("r:{s}"),
            });
            true
        })
        .unwrap();
        assert_eq!(reply.content, "Hello", "the done message is not appended twice");
        assert_eq!(seen, ["r:thinking", "c:Hel", "c:lo"]);
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "call_9");
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        let u = reply.usage.unwrap();
        assert_eq!((u.prompt, u.completion, u.cache_read), (120, 7, 100));
        assert_eq!(reply.raw_key, ITEMS_KEY);
        let raw = reply.raw.unwrap();
        assert_eq!(raw[0]["encrypted_content"], "enc");
        assert!(raw[0]["status"].is_null(), "only what goes back");
    }

    #[test]
    fn stream_errors_and_length_are_reported() {
        let resp = stream(&[
            json!({"type": "response.failed", "response": {"error": {"code": "rate_limit_exceeded", "message": "slow down"}}}),
        ]);
        let err = parse_stream(resp, &mut |_| true).unwrap_err().to_string();
        assert!(err.contains("slow down") && err.contains("rate_limit_exceeded"), "{err}");

        let resp = stream(&[
            json!({"type": "response.output_text.delta", "delta": "partial"}),
            json!({"type": "response.incomplete", "response": {"incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 1, "output_tokens": 2}}}),
        ]);
        let reply = parse_stream(resp, &mut |_| true).unwrap();
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
        assert_eq!(reply.content, "partial");
    }

    #[test]
    fn eof_without_terminal_event_is_interrupted_before_or_after_text() {
        let resp = stream(&[json!({"type": "response.created", "response": {}})]);
        let err = parse_stream(resp, &mut |_| true).unwrap_err().to_string();
        assert!(err.contains("stream interrupted") && err.contains("terminal event"), "{err}");

        let resp = stream(&[json!({"type": "response.output_text.delta", "delta": "partial answer"})]);
        let mut shown = String::new();
        let err = parse_stream(resp, &mut |delta| {
            if let Delta::Content(text) = delta {
                shown.push_str(text);
            }
            true
        })
        .unwrap_err()
        .to_string();
        assert_eq!(shown, "partial answer");
        assert!(err.contains("stream interrupted") && err.contains("terminal event"), "{err}");
    }

    #[test]
    fn cancellation_on_a_complete_message_stops_before_later_items() {
        let resp = stream(&[
            json!({"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call", "call_id": "call_1", "name": "bash", "arguments": "{}"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ]);
        let reply = parse_stream(resp, &mut |_| false).unwrap();
        assert_eq!(reply.content, "answer");
        assert_eq!(reply.finish_reason.as_deref(), Some("interrupted"));
        assert!(reply.tool_calls.is_empty(), "cancellation must stop before later tool calls");
    }

    #[test]
    fn configured_model_reads_quoted_values_up_to_the_closing_quote() {
        for (text, want) in [
            ("model = \"gpt-x\"\n", Some("gpt-x")),
            ("model = \"gpt-x\"# note\n", Some("gpt-x")),
            ("model = 'gpt-x' # note\n", Some("gpt-x")),
            ("model = gpt-x # note\n", Some("gpt-x")),
            ("[profiles.a]\nmodel = \"gpt-y\"\n", None),
        ] {
            assert_eq!(model_in(text).as_deref(), want, "{text}");
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(base64url("aGVsbG8").unwrap(), b"hello");
        assert_eq!(base64url("aGVsbG8=").unwrap(), b"hello");
        assert!(base64url("not base64!").is_none());
        let stamp = rfc3339_now();
        assert_eq!(stamp.len(), 20, "{stamp}");
        assert!(stamp.starts_with("20") && stamp.ends_with('Z'));
        let (a, b) = (session_id(), session_id());
        assert_eq!(a.len(), 36, "{a}");
        assert_eq!(a.matches('-').count(), 4, "{a}");
        assert_ne!(a, b);
    }
}
