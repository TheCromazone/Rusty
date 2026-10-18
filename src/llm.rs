//! Chat client with SSE streaming, tool calls and key rotation, for
//! OpenAI-compatible endpoints (NVIDIA, OpenAI, local servers), Anthropic's
//! Messages API and the Codex backend behind a ChatGPT sign-in. Each request
//! goes to the provider its model belongs to.

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Response;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::anthropic;
use crate::codex;
use crate::config::Provider;
use crate::execution::ExecutionMode;
use crate::sse::{Field, First, Obj, Str};
use crate::ui::truncate;

/// How long a request keeps retrying rate limits and server errors before it
/// gives up, unless `RUSTY_RETRY_SECS` says otherwise. Long enough to ride out
/// a free tier's per-minute limit with several agents sharing one key.
const RETRY_SECS: u64 = 300;
/// The longest single wait, whatever a Retry-After header asks for.
const MAX_WAIT_SECS: u64 = 120;
/// How long a request may go quiet, waiting for headers or for the next
/// bytes of the body, before it counts as dropped and is retried, unless
/// `RUSTY_STREAM_IDLE_SECS` says otherwise. Five minutes, as Codex waits.
const STREAM_IDLE_SECS: u64 = 300;

fn stream_idle() -> Duration {
    let secs = std::env::var("RUSTY_STREAM_IDLE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(STREAM_IDLE_SECS);
    Duration::from_secs(secs.clamp(1, 3600))
}

thread_local! {
    /// Where this thread's request reports a retry wait, when someone is watching.
    static STATUS: std::cell::RefCell<Option<Sender<Event>>> = const { std::cell::RefCell::new(None) };
    /// Streamed text this thread has parsed but not yet sent (see `Held`).
    static HELD: std::cell::RefCell<Option<Held>> = const { std::cell::RefCell::new(None) };
}

/// Tells whoever started this request what it is waiting for; silent otherwise.
/// False when the one who started it has stopped listening (Ctrl-C, a
/// budget stop), so a retry nobody will read is not sent.
fn status(text: String) -> bool {
    send_held();
    STATUS.with(|s| s.borrow().as_ref().is_none_or(|tx| tx.send(Event::Status(text)).is_ok()))
}

/// Deltas on their way to the UI thread, joined into one event. A fast
/// stream parses many deltas from each read, and an event apiece cost an
/// allocation, a channel message and a wake-up of the UI thread per token.
/// What is held goes out before the stream next reads from the network, so
/// text never waits on a slow server: the first token is sent as soon as
/// its read has been parsed.
struct Held {
    tx: Sender<Event>,
    reasoning: bool,
    text: String,
    /// The receiver has gone; the stream should stop.
    gone: bool,
}

impl Held {
    fn push(&mut self, d: Delta) -> bool {
        let (reasoning, s) = match d {
            Delta::Reasoning(s) => (true, s),
            Delta::Content(s) => (false, s),
        };
        if reasoning != self.reasoning {
            self.send();
            self.reasoning = reasoning;
        }
        self.text.push_str(s);
        !self.gone
    }

    fn send(&mut self) {
        if self.text.is_empty() || self.gone {
            return;
        }
        // The next batch is likely the same size; start it with that room.
        let room = self.text.len();
        let text = std::mem::replace(&mut self.text, String::with_capacity(room));
        let ev = if self.reasoning { Event::Reasoning(text) } else { Event::Content(text) };
        self.gone = self.tx.send(ev).is_err();
    }
}

fn send_held() {
    HELD.with(|h| {
        if let Some(h) = h.borrow_mut().as_mut() {
            h.send();
        }
    });
}

fn ensure_request_watched() -> Result<()> {
    if crate::signal::interrupted() {
        bail!("interrupted before the next request attempt");
    }
    if !status(String::new()) {
        bail!("nobody is waiting for this request any more");
    }
    Ok(())
}

fn refresh_codex_auth(budget: &crate::budget::Budget, refresh: impl FnOnce(Duration) -> Result<()>) -> Result<()> {
    let timeout =
        if budget.limited() { budget.remaining()?.min(codex::REFRESH_TIMEOUT) } else { codex::REFRESH_TIMEOUT };
    refresh(timeout)?;
    // Refresh may have taken long enough for the request owner to cancel; do
    // not send the model request after its receiver has gone away.
    ensure_request_watched()
}

/// The lines of a streamed response.
pub fn lines(resp: Response) -> crate::sse::Lines<Body> {
    crate::sse::Lines::new(Body(resp))
}

/// A streamed response body, read with the timer runtime entered so
/// reqwest's per-read timeout can fire.
pub struct Body(Response);

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Everything parsed so far reaches the UI before this read waits.
        send_held();
        let _runtime = stream_runtime().enter();
        self.0.read(buf)
    }
}

fn stream_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        // One shared timer worker, independent of CPU count and retry count.
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .expect("stream timer runtime")
    })
}

/// How hard a request tries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Retry {
    /// One pass over the keys: for checks that should answer quickly.
    Once,
    /// Keep backing off within the retry budget.
    Patient,
}

fn retry_budget() -> Duration {
    let secs = std::env::var("RUSTY_RETRY_SECS").ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(RETRY_SECS);
    Duration::from_secs(secs.min(3600))
}

/// The wait before retry round `round` (1-based): the server's Retry-After
/// when it gave one, else 2, 4, 8, 16, then 30 seconds, each with up to 25%
/// jitter so agents sharing a key don't all come back at the same moment.
fn backoff(round: u32, retry_after: Option<u64>) -> Duration {
    let base = retry_after.unwrap_or_else(|| 2u64.saturating_pow(round).min(30)).clamp(1, MAX_WAIT_SECS);
    let jitter = base * 250 * crate::ui::rand(1000) as u64 / 1000;
    Duration::from_millis(base * 1000 + jitter)
}

/// What a background request sends back to the UI thread.
pub enum Event {
    Reasoning(String),
    Content(String),
    /// What the request is waiting on, such as a retry; empty when it moves on.
    Status(String),
    Done(Result<Reply>),
}

pub enum Delta<'a> {
    Reasoning(&'a str),
    Content(&'a str),
}

#[derive(Default, Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Usage {
    /// Every input token the request was charged for, cached ones included.
    pub prompt: u64,
    pub completion: u64,
    /// Input tokens served from the provider's prompt cache (part of `prompt`).
    pub cache_read: u64,
    /// Input tokens written to the prompt cache (Anthropic; part of `prompt`).
    pub cache_write: u64,
    /// What the provider says the request cost, when it says (OpenRouter's
    /// `usage.cost`, in US dollars).
    pub cost: Option<f64>,
}

#[derive(Default, Debug)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    /// Provider-private blocks for this turn (Anthropic's content blocks,
    /// Codex's reasoning items), kept on the assistant message under
    /// `raw_key` so they go back to the API unchanged. None otherwise.
    pub raw: Option<Value>,
    pub raw_key: &'static str,
    /// The output stopped because the context window was full (Anthropic's
    /// `model_context_window_exceeded`); `finish_reason` still says "length".
    pub window_full: bool,
}

/// One provider's endpoint and its keys.
pub struct Endpoint {
    pub provider: Provider,
    pub base_url: String,
    keys: Vec<String>,
    key_idx: AtomicUsize,
    /// The ChatGPT sign-in, for the Codex provider; refreshed in place.
    codex: Option<Mutex<codex::Auth>>,
    /// Sent as Codex's `session_id` header. The backend keys its prompt
    /// cache on it and, without one, gives every request a fresh key, so
    /// nothing is ever served from the cache.
    session: String,
}

impl Endpoint {
    pub fn new(provider: Provider, base_url: String, keys: Vec<String>) -> Self {
        Self {
            provider,
            base_url: base_url.trim_end_matches('/').to_string(),
            keys,
            key_idx: AtomicUsize::new(0),
            codex: None,
            session: String::new(),
        }
    }

    /// The Codex backend with a saved sign-in instead of keys.
    pub fn codex(base_url: String, auth: codex::Auth) -> Self {
        let mut ep = Self::new(Provider::Codex, base_url, Vec::new());
        ep.codex = Some(Mutex::new(auth));
        ep.session = codex::session_id();
        ep
    }

    /// Whether requests can be authenticated: a key, or a sign-in.
    pub fn usable(&self) -> bool {
        !self.keys.is_empty() || self.codex.is_some()
    }

    /// The signed-in account's email, for the doctor (Codex only).
    pub fn account(&self) -> Option<String> {
        self.codex.as_ref()?.lock().ok()?.email.clone()
    }
}

pub struct Client {
    http: reqwest::blocking::Client,
    endpoints: Vec<Endpoint>,
    pub budget: Arc<crate::budget::Budget>,
}

struct Sent {
    response: Response,
    reservation: Option<crate::budget::Reservation>,
}

impl Client {
    /// Every provider with at least one key in the environment.
    pub fn from_env() -> Result<Self> {
        let endpoints = Provider::ALL
            .into_iter()
            .filter_map(|p| match p {
                Provider::Codex => codex::Auth::load().map(|auth| Endpoint::codex(p.base_url(), auth)),
                p => Some(Endpoint::new(p, p.base_url(), p.keys())),
            })
            .filter(Endpoint::usable)
            .collect();
        Self::new(endpoints)
    }

    pub fn new(endpoints: Vec<Endpoint>) -> Result<Self> {
        if !endpoints.iter().any(Endpoint::usable) {
            bail!(
                "no API key found. Set NVIDIA_API_KEY, ANTHROPIC_API_KEY or OPENAI_API_KEY in your shell, \
                 in ~/.config/rusty/.env, or in the .env next to rusty's Cargo.toml; or sign in with `codex login` \
                 to use your ChatGPT account"
            );
        }
        // A per-read idle limit, and no total deadline: a long answer that keeps
        // streaming is never cut off. Budgeted runs set their own deadline below.
        let http = reqwest::blocking::ClientBuilder::from(
            reqwest::Client::builder().connect_timeout(Duration::from_secs(20)).read_timeout(stream_idle()),
        )
        // The blocking builder has its own 30-second timeout, separate from
        // the async client's per-read timeout. Leave it disabled so the
        // configured idle limit governs unbudgeted requests; budgeted
        // requests still set an explicit per-request deadline in `send_with`.
        .timeout(None)
        .build()?;
        Ok(Self { http, endpoints, budget: crate::budget::Budget::unbounded() })
    }

    /// Limits, a ledger that outlives the process, or both.
    pub fn with_budget(
        mut self,
        limits: Option<crate::budget::Limits>,
        ledger: Option<&std::path::Path>,
    ) -> Result<Self> {
        self.budget = crate::budget::Budget::open(limits, ledger)?;
        Ok(self)
    }

    pub fn key_count(&self) -> usize {
        self.endpoints.iter().map(|e| e.keys.len().max(usize::from(e.codex.is_some()))).sum()
    }

    pub fn endpoints(&self) -> &[Endpoint] {
        &self.endpoints
    }

    /// The endpoint that serves `model`. An OpenAI model with no OpenAI key
    /// goes to the Codex sign-in when there is one. A model whose provider
    /// has no key goes to a custom compatible endpoint (`RUSTY_BASE_URL`,
    /// say a local router) when there is one, since that is where it was
    /// meant to go.
    pub fn endpoint_for(&self, model: &str) -> Result<&Endpoint> {
        let want = Provider::for_model(model);
        if let Some(e) = self.endpoints.iter().find(|e| e.provider == want) {
            return Ok(e);
        }
        if want == Provider::OpenAi {
            if let Some(e) = self.endpoints.iter().find(|e| e.provider == Provider::Codex) {
                return Ok(e);
            }
        }
        if let Some(e) = self
            .endpoints
            .iter()
            .find(|e| e.provider == Provider::Compatible && e.base_url != crate::config::DEFAULT_BASE_URL)
        {
            return Ok(e);
        }
        if want == Provider::Codex {
            bail!("`{model}` needs the Codex sign-in, but none was found. Run `codex login`, or pick another model with /model")
        }
        let vars = want.key_vars();
        bail!(
            "`{model}` is served by {}, but {} is not set. Add it to your shell or ~/.config/rusty/.env, \
             {}or pick another model with /model",
            want.name(),
            vars.iter().find(|v| !v.starts_with("RUSTY_")).unwrap_or(&vars[0]),
            if want == Provider::OpenAi { "run `codex login` to use your ChatGPT account, " } else { "" }
        )
    }

    /// Runs a chat request on a background thread and streams events back, so
    /// the caller can animate and react to Ctrl-C while waiting. Dropping the
    /// receiver stops the stream. The messages are shared rather than copied:
    /// the thread lets go of them before it sends `Done`, so a caller that
    /// holds the other handle can take them back without a copy.
    pub fn start_chat(
        self: &Arc<Self>,
        model: String,
        messages: impl Into<Arc<Vec<Value>>>,
        tools: Value,
        temperature: f32,
        inference: (ExecutionMode, &'static str, Option<u32>),
    ) -> Receiver<Event> {
        let (tx, rx) = channel();
        let me = self.clone();
        let messages = messages.into();
        std::thread::spawn(move || {
            STATUS.with(|s| *s.borrow_mut() = Some(tx.clone()));
            let held = Held { tx: tx.clone(), reasoning: false, text: String::new(), gone: false };
            HELD.with(|h| *h.borrow_mut() = Some(held));
            let result = me.chat(&model, &messages, &tools, temperature, inference, &mut |d| {
                HELD.with(|h| h.borrow_mut().as_mut().is_none_or(|h| h.push(d)))
            });
            send_held();
            drop(messages);
            let _ = tx.send(Event::Done(result));
        });
        rx
    }

    /// Streams one chat completion, calling `on` for every text/reasoning
    /// delta; `on` returns false to stop early. An empty `tools` array means a
    /// plain completion. `inference` is the mode, the request's role, and the
    /// most output it may ask for (see [`clamp_output`]).
    pub fn chat(
        &self,
        model: &str,
        messages: &[Value],
        tools: &Value,
        temperature: f32,
        inference: (ExecutionMode, &str, Option<u32>),
        on: &mut dyn FnMut(Delta) -> bool,
    ) -> Result<Reply> {
        let (execution_mode, role, output_cap) = inference;
        let ep = self.endpoint_for(model)?;
        if ep.provider == Provider::Anthropic {
            let first_party = ep.base_url == crate::config::ANTHROPIC_BASE_URL;
            let (mut body, beta) = anthropic::request(model, messages, tools, execution_mode, first_party);
            clamp_output(&mut body, output_cap);
            let url = format!("{}/v1/messages", ep.base_url);
            return self.stream_with_retry(on, |on| {
                let sent = self.send(ep, &url, Some(Payload::of(&body)), beta.as_deref(), role)?;
                let result = anthropic::parse_stream(sent.response, on);
                settle(sent.reservation, &result);
                result
            });
        }
        if ep.provider == Provider::Codex {
            let mut body = codex::request(model, messages, tools, execution_mode);
            clamp_output(&mut body, output_cap);
            let url = format!("{}/codex/responses", ep.base_url);
            return self.stream_with_retry(on, |on| {
                let sent = self.send(ep, &url, Some(Payload::of(&body)), None, role)?;
                let result = codex::parse_stream(sent.response, on);
                settle(sent.reservation, &result);
                result
            });
        }
        // The messages are written from history when the body is serialised;
        // the placeholder keeps their place among the sorted keys.
        let mut body = json!({
            "model": model,
            "messages": null,
            "stream": true,
            "stream_options": { "include_usage": true },
            "max_tokens": 16384,
            "temperature": temperature,
        });
        if ep.provider == Provider::OpenAi {
            openai_inference(&mut body, model, execution_mode);
        } else {
            execution_mode.apply_inference(&mut body, &ep.base_url, model);
        }
        clamp_output(&mut body, output_cap);
        if tools.as_array().is_some_and(|t| !t.is_empty()) {
            body["tools"] = tools.clone();
            body["tool_choice"] = json!("auto");
        }
        let url = format!("{}/chat/completions", ep.base_url);
        self.stream_with_retry(on, |on| {
            let sent = self.send(ep, &url, Some(Payload { value: &body, messages: Some(messages) }), None, role)?;
            let result = parse_stream(sent.response, on);
            settle(sent.reservation, &result);
            result
        })
    }

    /// Runs one streamed request, and runs it again when the provider reports
    /// an overload, rate limit or server error inside the stream (an HTTP 200
    /// whose events carry the error) or the stream drops, as long as nothing
    /// has been shown yet. Once answer text has streamed, a retry would repeat
    /// it, so the error stands. Shares the retry budget and backoff with `send`.
    fn stream_with_retry(
        &self,
        on: &mut dyn FnMut(Delta) -> bool,
        mut attempt: impl FnMut(&mut dyn FnMut(Delta) -> bool) -> Result<Reply>,
    ) -> Result<Reply> {
        let started = std::time::Instant::now();
        let budget = retry_budget();
        let mut round = 0u32;
        loop {
            self.budget.remaining()?;
            let mut shown = false;
            let result = attempt(&mut |d| {
                // Reasoning is neither kept in history nor shown outside the
                // verbose view, so a drop while the model thinks is retryable.
                shown |= matches!(d, Delta::Content(_));
                on(d)
            });
            let err = match result {
                Err(e) if !shown && transient_stream_error(&format!("{e:#}")) => e,
                other => return other,
            };
            round += 1;
            let wait = backoff(round, None);
            if started.elapsed() + wait > budget {
                return Err(err.context(format!("giving up after {round} streamed attempts")));
            }
            self.wait_retry(wait, &format!("{err:#}"))?;
        }
    }

    /// Model ids from every configured provider. A provider that fails to
    /// answer is skipped unless none answer.
    pub fn list_models(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let mut last_err = None;
        for ep in &self.endpoints {
            match self.models_of(ep, Retry::Patient) {
                Ok(found) => ids.extend(found),
                Err(e) => last_err = Some(e),
            }
        }
        if ids.is_empty() {
            if let Some(e) = last_err {
                return Err(e);
            }
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// One provider's model ids. The doctor asks with `Retry::Once`, so a bad
    /// key answers in a second instead of after the whole retry budget.
    pub fn models_of(&self, ep: &Endpoint, retry: Retry) -> Result<Vec<String>> {
        if ep.provider == Provider::Codex {
            // The backend publishes no catalog to ask; Codex saves the one it was
            // shown, and the configured model is known to work either way.
            let mut ids = codex::listed_models();
            let chosen = Provider::Codex.default_model();
            if !ids.contains(&chosen) {
                ids.insert(0, chosen);
            }
            return Ok(ids);
        }
        let url = match ep.provider {
            Provider::Anthropic => format!("{}/v1/models?limit=1000", ep.base_url),
            _ => format!("{}/models", ep.base_url),
        };
        let _runtime = stream_runtime().enter();
        let v: Value = self
            .send_with(ep, &url, (None, "catalog"), None, retry)?
            .response
            .json()
            .context("bad /models response")?;
        let data = v["data"].as_array().ok_or_else(|| anyhow!("unexpected /models response"))?;
        let mut ids: Vec<String> = data.iter().filter_map(|m| m["id"].as_str().map(String::from)).collect();
        ids.sort();
        Ok(ids)
    }

    /// The context window an endpoint publishes for `model` in its model list
    /// (`context_length` on OpenRouter, `max_model_len` on vLLM), asked once.
    /// None when the provider publishes none (Anthropic) or can't be reached.
    pub fn context_window(&self, model: &str) -> Option<usize> {
        let ep = self.endpoint_for(model).ok()?;
        if matches!(ep.provider, Provider::Anthropic | Provider::Codex) {
            return None;
        }
        let url = format!("{}/models", ep.base_url);
        let _runtime = stream_runtime().enter();
        let v: Value = self.send_with(ep, &url, (None, "catalog"), None, Retry::Once).ok()?.response.json().ok()?;
        let entry = v["data"].as_array()?.iter().find(|m| m["id"].as_str() == Some(model))?;
        ["context_length", "max_model_len", "context_window", "max_context_length"]
            .iter()
            .find_map(|k| entry[*k].as_u64())
            .map(|n| n as usize)
            .filter(|n| *n > 0)
    }

    /// Sends a request, rotating keys and backing off on rate limits, server
    /// errors and dropped connections until the retry budget runs out
    /// (`RUSTY_RETRY_SECS`, default five minutes). A key that is refused (401
    /// or 403) twice is not worth waiting on, so auth failures give up once
    /// every key has been refused twice. Ctrl-C ends a wait at once.
    fn send(&self, ep: &Endpoint, url: &str, body: Option<Payload>, beta: Option<&str>, role: &str) -> Result<Sent> {
        self.send_with(ep, url, (body, role), beta, Retry::Patient)
    }

    fn send_with(
        &self,
        ep: &Endpoint,
        url: &str,
        request: (Option<Payload>, &str),
        beta: Option<&str>,
        retry: Retry,
    ) -> Result<Sent> {
        // The async read timeout is polled while blocking responses are
        // consumed here too: error bodies and Codex refresh bodies need the
        // same timer context as streamed and catalog responses.
        let _runtime = stream_runtime().enter();
        let (body, role) = request;
        // Serialised once: the same bytes size the reservation and go out on
        // every attempt.
        let payload = body.map(|b| b.encode()).transpose()?;
        let reserve = payload.as_ref().map(|(_, n)| *n);
        let keys = &ep.keys;
        // A sign-in is one slot that refreshes itself instead of rotating.
        let slots = keys.len().max(usize::from(ep.codex.is_some()));
        let started = std::time::Instant::now();
        let budget = if retry == Retry::Once { Duration::ZERO } else { retry_budget() };
        let mut refused = vec![0u32; slots];
        let mut last_err = String::new();
        let mut retry_after: Option<u64> = None;
        let mut attempts = 0usize;
        let mut round = 0u32;
        loop {
            for _ in 0..slots {
                ensure_request_watched()?;
                let idx = ep.key_idx.load(Ordering::Relaxed) % slots;
                let mut req = match &payload {
                    Some((bytes, _)) => self
                        .http
                        .post(url)
                        .header(reqwest::header::CONTENT_TYPE, "application/json")
                        .body(bytes.clone()),
                    None => self.http.get(url),
                };
                req = match &ep.codex {
                    Some(auth) => {
                        let mut auth = auth.lock().map_err(|_| anyhow!("codex sign-in poisoned"))?;
                        // Refused once, or about to expire: refresh before trying again.
                        if auth.expiring() || refused[idx] > 0 {
                            refresh_codex_auth(&self.budget, |timeout| auth.refresh(&self.http, timeout))?;
                        }
                        req.bearer_auth(auth.access_token())
                            .header("chatgpt-account-id", auth.account_id())
                            .header("OpenAI-Beta", "responses=experimental")
                            .header("originator", codex_originator())
                            .header("session_id", &ep.session)
                    }
                    None if ep.provider == Provider::Anthropic => {
                        req.header("x-api-key", &keys[idx]).header("anthropic-version", anthropic::VERSION)
                    }
                    None => req.bearer_auth(&keys[idx]),
                };
                if let Some(b) = beta {
                    req = req.header("anthropic-beta", b);
                }
                // Auth lookup can wait on another request's refresh while
                // holding the endpoint mutex. Recheck after that wait so a
                // canceled owner never starts a model request afterward.
                ensure_request_watched()?;
                let mut reservation = reserve.map(|n| self.budget.reserve(role, n)).transpose()?;
                if reservation.is_some() && self.budget.limited() {
                    req = req.timeout(self.budget.remaining()?);
                }
                attempts += 1;
                match req.send() {
                    Ok(r) if r.status().is_success() => {
                        if reservation.is_some() {
                            self.budget.note_ok();
                        }
                        return Ok(Sent { response: r, reservation });
                    }
                    Ok(r) => {
                        let status = r.status().as_u16();
                        if let Some(secs) = r
                            .headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.trim().parse::<u64>().ok())
                        {
                            retry_after = Some(retry_after.unwrap_or(0).max(secs));
                        }
                        let text = r.text().unwrap_or_default();
                        // The JSON error's message reads better in the retry footer than the raw body.
                        let message = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
                            [&v["error"]["message"], &v["detail"], &v["message"]]
                                .iter()
                                .find_map(|m| m.as_str().map(String::from))
                        });
                        last_err = format!(
                            "HTTP {}: {}",
                            r_status(status),
                            truncate(message.as_deref().unwrap_or(text.trim()), 400)
                        );
                        match status {
                            401 | 403 => refused[idx] += 1,
                            // A ChatGPT plan's usage limit lasts hours; waiting it out here only stalls.
                            429 if text.contains("usage_limit_reached") || text.contains("usage_not_included") => {
                                bail!(last_err)
                            }
                            429 => {
                                if let Some(r) = reservation.take() {
                                    r.refund_rate_limited();
                                }
                            }
                            500..=599 => {}
                            _ => bail!(last_err),
                        }
                    }
                    Err(e) => {
                        if reservation.is_some() {
                            self.budget.note_transport_error();
                        }
                        last_err = format!("request failed: {e}");
                    }
                }
                ep.key_idx.store((idx + 1) % slots, Ordering::Relaxed);
            }
            round += 1;
            let wait = backoff(round, retry_after.take());
            let all_refused = refused.iter().all(|n| *n >= 2);
            if all_refused || started.elapsed() + wait > budget {
                bail!("giving up after {attempts} attempts in {}s: {last_err}", started.elapsed().as_secs());
            }
            if body.is_some() {
                self.wait_retry(wait, &last_err)?;
            } else {
                wait_or_interrupt(wait, &last_err)?;
            }
        }
    }
    fn wait_retry(&self, wait: Duration, last_err: &str) -> Result<()> {
        let started = std::time::Instant::now();
        let waited = wait_or_interrupt(wait.min(self.budget.remaining()?), last_err);
        self.budget.note_wait(started.elapsed());
        waited?;
        self.budget.remaining()?;
        Ok(())
    }
}

/// Lowers the request's output cap to `cap`, the room the window leaves
/// beside the prompt, so prompt and reply together never ask for more than
/// the window. A reasoning budget keeps to three quarters of the new cap.
fn clamp_output(body: &mut Value, cap: Option<u32>) {
    let Some(cap) = cap.map(u64::from) else { return };
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if body[key].as_u64().is_some_and(|n| n > cap) {
            body[key] = json!(cap);
        }
    }
    if body["reasoning_budget"].as_u64().is_some_and(|n| n > cap * 3 / 4) {
        body["reasoning_budget"] = json!(cap * 3 / 4);
    }
}

/// A request body to send. `messages`, when set, are written in place of the
/// body's `messages` key, without rusty's private keys (see
/// [`anthropic::Public`]), so a long history is never copied to be sent.
#[derive(Clone, Copy)]
struct Payload<'a> {
    value: &'a Value,
    messages: Option<&'a [Value]>,
}

impl<'a> Payload<'a> {
    fn of(value: &'a Value) -> Self {
        Self { value, messages: None }
    }

    /// The JSON bytes and the tokens the budget reserves for them: their
    /// length plus the output cap.
    fn encode(self) -> Result<(Vec<u8>, u64)> {
        let body = self.value;
        let output = body["max_completion_tokens"]
            .as_u64()
            .or_else(|| body["max_tokens"].as_u64())
            .or_else(|| body["max_output_tokens"].as_u64())
            .ok_or_else(|| anyhow!("model request has no output token cap"))?;
        let bytes = serde_json::to_vec(&self)?;
        let reserve = (bytes.len() as u64)
            .checked_add(output)
            .ok_or_else(|| anyhow!("model budget exhausted: request reservation overflow"))?;
        Ok((bytes, reserve))
    }
}

impl serde::Serialize for Payload<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (Some(map), Some(messages)) = (self.value.as_object(), self.messages) else {
            return self.value.serialize(s);
        };
        let mut out = s.serialize_map(Some(map.len()))?;
        for (k, v) in map {
            if k == "messages" {
                out.serialize_entry(k, &anthropic::Public(messages))?;
            } else {
                out.serialize_entry(k, v)?;
            }
        }
        out.end()
    }
}

fn settle(reservation: Option<crate::budget::Reservation>, result: &Result<Reply>) {
    if let (Some(r), Ok(reply)) = (reservation, result) {
        if reply.finish_reason.as_deref().is_some_and(|f| f != "interrupted") {
            if let Some(u) = reply.usage {
                r.settle(u.prompt, u.completion, u.cost);
            }
        }
    }
}

/// Sleeps for `wait`, or fails at once when Ctrl-C is pressed.
fn wait_or_interrupt(wait: Duration, last_err: &str) -> Result<()> {
    let until = std::time::Instant::now() + wait;
    let why = truncate(last_err.lines().next().unwrap_or(last_err), 60);
    // Without a footer to count down in, say once what the wait is for.
    if !crate::ui::tty() {
        eprintln!("retrying in {}s · {why}", wait.as_secs().max(1));
    }
    let mut shown = u64::MAX;
    while std::time::Instant::now() < until {
        if crate::signal::interrupted() {
            status(String::new());
            bail!("interrupted while waiting to retry: {last_err}");
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.as_secs() != shown {
            shown = left.as_secs();
            if !status(format!("retrying in {}s · {why}", shown + 1)) {
                bail!("nobody is waiting for this request any more: {last_err}");
            }
        }
        std::thread::sleep(Duration::from_millis(100).min(left));
    }
    status(String::new());
    Ok(())
}

/// An error inside a stream worth trying again: overloaded, rate limited, a
/// server-side failure, or the connection dropping. A bad request is not.
fn transient_stream_error(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    if e.contains("stream interrupted") {
        return true;
    }
    if !e.contains("api error") {
        return false;
    }
    [
        "overloaded",
        "service_unavailable",
        "server_error",
        "rate_limit",
        "api_error",
        "timeout",
        "\"code\":429",
        "\"code\":5",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// "429 Too Many Requests" style, as reqwest prints a status.
fn r_status(code: u16) -> String {
    reqwest::StatusCode::from_u16(code).map(|s| s.to_string()).unwrap_or_else(|_| code.to_string())
}

/// What the Codex backend is told is calling it. It serves the Codex CLI,
/// so that is the default; `RUSTY_CODEX_ORIGINATOR` names rusty instead.
fn codex_originator() -> String {
    std::env::var("RUSTY_CODEX_ORIGINATOR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "codex_cli_rs".into())
}

/// OpenAI's own API: reasoning models (gpt-5, o-series) take
/// `max_completion_tokens` and `reasoning_effort` and reject a temperature.
fn openai_inference(body: &mut Value, model: &str, mode: ExecutionMode) {
    let m = model.to_ascii_lowercase();
    let reasoning = m.starts_with("gpt-5")
        || m.starts_with("codex-")
        || (m.starts_with('o') && m[1..].starts_with(|c: char| c.is_ascii_digit()));
    if let Some(o) = body.as_object_mut() {
        let cap = o.remove("max_tokens").unwrap_or(json!(16384));
        o.insert(
            "max_completion_tokens".into(),
            json!(u64::from(mode.max_tokens()).max(cap.as_u64().unwrap_or(16384))),
        );
        if reasoning {
            o.remove("temperature");
            let effort = match mode {
                ExecutionMode::Careful => "high",
                ExecutionMode::Standard => "medium",
                ExecutionMode::Vibe => "low",
            };
            o.insert("reasoning_effort".into(), json!(effort));
        }
    }
}

/// One streamed chat-completions chunk, read in place (see `crate::sse`).
#[derive(Deserialize, Default)]
#[serde(default)]
struct Chunk<'a> {
    error: Field,
    usage: Field,
    #[serde(borrow)]
    choices: First<Obj<Choice<'a>>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Choice<'a> {
    #[serde(borrow)]
    finish_reason: Str<'a>,
    #[serde(borrow)]
    delta: Obj<ChunkDelta<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ChunkDelta<'a> {
    #[serde(borrow)]
    reasoning_content: Str<'a>,
    #[serde(borrow)]
    reasoning: Str<'a>,
    #[serde(borrow)]
    content: Str<'a>,
    tool_calls: Field,
}

fn parse_stream(resp: Response, on: &mut dyn FnMut(Delta) -> bool) -> Result<Reply> {
    let mut reply = Reply::default();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut saw_data = false;
    let mut done = false;
    let mut raw = String::new();

    let mut lines = lines(resp);
    while let Some(line) = lines.next_line() {
        let line = line.context("stream interrupted")?;
        let Some(data) = line.strip_prefix("data:") else {
            if !saw_data {
                raw.push_str(line);
            }
            continue;
        };
        saw_data = true;
        let data = data.trim();
        if data == "[DONE]" {
            done = true;
            break;
        }
        let mut spare = None;
        let Some(chunk) = crate::sse::event::<Chunk>(data, &mut spare) else { continue };
        if let Some(err) = chunk.error.0 {
            bail!("API error: {err}");
        }
        if let Some(u) = chunk.usage.0.filter(|u| !u.is_null()) {
            reply.usage = usage_of(&u);
        }
        let Some(Obj(choice)) = chunk.choices.0 else { continue };
        if let Some(fr) = choice.finish_reason.get() {
            reply.finish_reason = Some(fr.to_string());
        }
        let delta = choice.delta.0;
        // Providers name this field differently; take the first one present.
        if let Some(s) = delta.reasoning_content.get().or(delta.reasoning.get()) {
            if !s.is_empty() && !on(Delta::Reasoning(s)) {
                reply.finish_reason = Some("interrupted".into());
                break;
            }
        }
        if let Some(s) = delta.content.get() {
            if !s.is_empty() {
                reply.content.push_str(s);
                if !on(Delta::Content(s)) {
                    reply.finish_reason = Some("interrupted".into());
                    break;
                }
            }
        }
        if let Some(tcs) = delta.tool_calls.0.as_ref().and_then(Value::as_array) {
            for tc in tcs {
                merge_tool_call(&mut calls, tc);
            }
        }
    }

    if saw_data && !done && reply.finish_reason.is_none() {
        bail!("stream interrupted: it ended before the reply finished");
    }
    // Some servers ignore `stream: true` and answer with one JSON body.
    if !saw_data && !raw.trim().is_empty() {
        let v: Value =
            serde_json::from_str(&raw).with_context(|| format!("unexpected response: {}", truncate(&raw, 300)))?;
        let msg = &v["choices"][0]["message"];
        if let Some(s) = msg["content"].as_str() {
            reply.content = s.to_string();
            let _ = on(Delta::Content(s));
        }
        for tc in msg["tool_calls"].as_array().into_iter().flatten() {
            merge_tool_call(&mut calls, tc);
        }
        reply.finish_reason = v["choices"][0]["finish_reason"].as_str().map(String::from);
        reply.usage = v.get("usage").and_then(usage_of);
    }

    for c in &mut calls {
        // gpt-oss on some servers leaks its channel marker into the name
        // (`read_file<|channel|>commentary`); the tool is the part before it.
        if let Some(at) = c.name.find("<|") {
            c.name.truncate(at);
        }
    }
    calls.retain(|c| !c.name.is_empty());
    for (i, c) in calls.iter_mut().enumerate() {
        if c.id.is_empty() {
            c.id = format!("call_{i}");
        }
    }
    reply.tool_calls = calls;
    Ok(reply)
}

fn usage_of(u: &Value) -> Option<Usage> {
    Some(Usage {
        prompt: u["prompt_tokens"].as_u64()?,
        completion: u["completion_tokens"].as_u64()?,
        cache_read: u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
        cache_write: 0,
        cost: u["cost"].as_f64(),
    })
}

/// Folds one streamed tool-call fragment into the accumulated list.
fn merge_tool_call(calls: &mut Vec<ToolCall>, tc: &Value) {
    let idx = match tc["index"].as_u64() {
        Some(i) => i as usize,
        None => match tc["id"].as_str() {
            Some(id) => calls.iter().position(|c| c.id == id).unwrap_or(calls.len()),
            None => calls.len().saturating_sub(1),
        },
    };
    while calls.len() <= idx {
        calls.push(ToolCall::default());
    }
    let c = &mut calls[idx];
    if let Some(id) = tc["id"].as_str().filter(|s| !s.is_empty()) {
        c.id = id.to_string();
    }
    if let Some(name) = tc["function"]["name"].as_str().filter(|s| !s.is_empty()) {
        if c.name.is_empty() {
            c.name = name.to_string();
        }
    }
    match &tc["function"]["arguments"] {
        Value::String(s) => c.arguments.push_str(s),
        Value::Null => {}
        other => c.arguments.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_transient_stream_errors_are_retried() {
        for e in [
            r#"API error: {"code":503,"message":"Service temporarily overloaded","type":"service_unavailable"}"#,
            r#"API error: {"type":"overloaded_error","message":"Overloaded"}"#,
            r#"API error: {"code":429,"message":"slow down"}"#,
            "stream interrupted: connection reset",
        ] {
            assert!(transient_stream_error(e), "{e}");
        }
        for e in [
            r#"API error: {"code":400,"message":"bad tool schema","type":"invalid_request_error"}"#,
            "HTTP 404 Not Found: no such model",
        ] {
            assert!(!transient_stream_error(e), "{e}");
        }
    }

    #[test]
    fn held_deltas_go_out_joined_and_in_order() {
        let (tx, rx) = channel();
        let mut held = Held { tx, reasoning: false, text: String::new(), gone: false };
        for d in [Delta::Reasoning("a"), Delta::Reasoning("b"), Delta::Content("c"), Delta::Content("d")] {
            assert!(held.push(d));
        }
        held.send();
        assert!(held.push(Delta::Content("e")));
        held.send();
        held.send();
        let got: Vec<String> = rx
            .try_iter()
            .map(|e| match e {
                Event::Reasoning(s) => format!("r:{s}"),
                Event::Content(s) => format!("c:{s}"),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(got, ["r:ab", "c:cd", "c:e"]);
        drop(rx);
        held.push(Delta::Content("f"));
        held.send();
        assert!(!held.push(Delta::Content("g")), "a closed receiver stops the stream");
    }

    #[test]
    fn backoff_grows_then_caps_and_honours_retry_after() {
        for round in 1..8 {
            let d = backoff(round, None).as_millis() as u64;
            let base = 2u64.pow(round).min(30) * 1000;
            assert!(d >= base && d <= base + base / 4, "round {round}: {d}");
        }
        assert!(backoff(1, Some(90)).as_secs() >= 90);
        assert!(backoff(1, Some(10_000)).as_secs() <= MAX_WAIT_SECS + MAX_WAIT_SECS / 4);
        // A server that says "retry after 0" is not hammered in a loop.
        assert!(backoff(3, Some(0)) >= Duration::from_secs(1));
        let spread = (0..50).map(|_| backoff(3, None).as_millis()).max().unwrap() - 8000;
        assert!(spread > 100, "jitter spreads retries by up to a quarter: {spread}ms");
    }

    #[test]
    fn a_closed_request_receiver_stops_key_rotation() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(2));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // On macOS the accepted socket inherits the listener's non-blocking
            // flag, so a read before the request lands fails with WouldBlock.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            // Let the request owner drop its event receiver before provoking
            // the immediate retry onto the second API key.
            std::thread::sleep(Duration::from_millis(200));
            write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            stream.flush().unwrap();

            let until = std::time::Instant::now() + Duration::from_millis(300);
            let mut count = 1;
            while std::time::Instant::now() < until {
                match listener.accept() {
                    Ok((mut next, _)) => {
                        count += 1;
                        let _ =
                            write!(next, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        let _ = next.flush();
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            count
        });

        let client =
            Client::new(vec![Endpoint::new(Provider::OpenAi, url.clone(), vec!["first".into(), "second".into()])])
                .unwrap();
        let endpoint = &client.endpoints[0];
        let (tx, rx) = channel();
        STATUS.with(|s| *s.borrow_mut() = Some(tx));
        let dropper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(rx);
        });
        let result = client.send_with(endpoint, &url, (None, "catalog"), None, Retry::Once);
        STATUS.with(|s| *s.borrow_mut() = None);
        dropper.join().unwrap();
        assert!(result.is_err());
        assert_eq!(server.join().unwrap(), 1, "do not rotate keys after the request owner disconnects");
    }

    #[test]
    fn a_cancelled_owner_does_not_send_after_codex_refresh() {
        let budget = crate::budget::Budget::unbounded();
        let (tx, rx) = channel();
        STATUS.with(|s| *s.borrow_mut() = Some(tx));
        let dropper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(rx);
        });
        let mut refresh_ran = false;
        let result = refresh_codex_auth(&budget, |_| {
            std::thread::sleep(Duration::from_millis(100));
            refresh_ran = true;
            Ok(())
        });
        STATUS.with(|s| *s.borrow_mut() = None);
        dropper.join().unwrap();
        assert!(refresh_ran);
        assert!(result.is_err(), "cancelled request must stop before the model POST");
    }

    #[test]
    fn a_cancelled_waiter_does_not_post_after_another_refreshes() {
        use std::io::Write;
        use std::net::TcpListener;

        fn jwt(exp: u64) -> String {
            let bytes = format!("{{\"exp\":{exp}}}");
            let table = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut encoded = String::new();
            for chunk in bytes.as_bytes().chunks(3) {
                let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
                for i in 0..chunk.len() + 1 {
                    encoded.push(table[((n >> (18 - 6 * i)) & 63) as usize] as char);
                }
            }
            format!("e30.{encoded}.sig")
        }

        let dir = std::env::temp_dir().join(format!("rusty-llm-refresh-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let expired_path = dir.join("expired.json");
        let fresh_path = dir.join("fresh.json");
        let expired_token = jwt(1);
        let fresh_token = jwt(4_102_444_800);
        std::fs::write(
            &expired_path,
            serde_json::json!({"tokens": {"access_token": expired_token, "refresh_token": "offline-old", "account_id": "offline-account"}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            &fresh_path,
            serde_json::json!({"tokens": {"access_token": fresh_token, "refresh_token": "offline-new", "account_id": "offline-account"}})
                .to_string(),
        )
        .unwrap();
        let auth = codex::Auth::load_from(expired_path).unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let until = std::time::Instant::now() + Duration::from_millis(350);
            let mut count = 0;
            while std::time::Instant::now() < until {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        count += 1;
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        let _ = stream.flush();
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("local fixture listener failed: {error}"),
                }
            }
            count
        });

        let client = Client::new(vec![Endpoint::codex(url.clone(), auth)]).unwrap();
        let endpoint = &client.endpoints[0];
        let auth_lock = endpoint.codex.as_ref().unwrap();
        let (locked_tx, locked_rx) = channel();
        let (release_tx, release_rx) = channel();
        let (status_tx, status_rx) = channel();
        let (result_tx, result_rx) = channel();

        std::thread::scope(|scope| {
            let holder = scope.spawn(move || {
                let mut guard = auth_lock.lock().unwrap();
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                *guard = codex::Auth::load_from(fresh_path).unwrap();
            });
            locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            let client = &client;
            let url = &url;
            let requester = scope.spawn(move || {
                STATUS.with(|s| *s.borrow_mut() = Some(status_tx));
                let result = client.send_with(endpoint, url, (None, "catalog"), None, Retry::Once);
                STATUS.with(|s| *s.borrow_mut() = None);
                result_tx.send(result.is_err()).unwrap();
            });
            // This empty status is the request's initial watcher check. It
            // then blocks on the auth mutex held above.
            status_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(status_rx);
            release_tx.send(()).unwrap();
            assert!(result_rx.recv_timeout(Duration::from_secs(2)).unwrap(), "cancelled request should stop");
            holder.join().unwrap();
            requester.join().unwrap();
        });

        assert_eq!(server.join().unwrap(), 0, "canceled waiter must not send a model POST");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_output_cap_never_exceeds_the_room_left() {
        let mut body = json!({"max_tokens": 32768, "reasoning_budget": 24576});
        clamp_output(&mut body, Some(32768));
        assert_eq!((body["max_tokens"].as_u64(), body["reasoning_budget"].as_u64()), (Some(32768), Some(24576)));
        clamp_output(&mut body, Some(8000));
        assert_eq!((body["max_tokens"].as_u64(), body["reasoning_budget"].as_u64()), (Some(8000), Some(6000)));
        let mut body = json!({"max_completion_tokens": 16384});
        clamp_output(&mut body, Some(4000));
        assert_eq!(body["max_completion_tokens"], 4000);
        assert!(body.get("max_tokens").is_none(), "no cap is added where none was sent");
        clamp_output(&mut body, None);
        assert_eq!(body["max_completion_tokens"], 4000);
    }

    #[test]
    fn openai_reasoning_models_get_their_own_parameters() {
        let mut body = json!({"max_tokens": 16384, "temperature": 0.3});
        openai_inference(&mut body, "gpt-5", ExecutionMode::Careful);
        assert!(body.get("temperature").is_none() && body.get("max_tokens").is_none());
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body["max_completion_tokens"].as_u64().unwrap() >= 16384);
        let mut body = json!({"max_tokens": 16384, "temperature": 0.3});
        openai_inference(&mut body, "gpt-4.1", ExecutionMode::Vibe);
        assert_eq!(body["temperature"], 0.3);
        assert!(body.get("reasoning_effort").is_none());
        let mut body = json!({"max_tokens": 16384});
        openai_inference(&mut body, "o4-mini", ExecutionMode::Vibe);
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn routes_each_model_to_its_provider() {
        let ep = |p: Provider, url: &str| Endpoint::new(p, url.into(), vec!["k".into()]);
        let c = Client::new(vec![
            ep(Provider::Compatible, crate::config::DEFAULT_BASE_URL),
            ep(Provider::Anthropic, crate::config::ANTHROPIC_BASE_URL),
        ])
        .unwrap();
        assert_eq!(c.endpoint_for("claude-opus-5-5").unwrap().provider, Provider::Anthropic);
        assert_eq!(c.endpoint_for("openai/gpt-oss-20b").unwrap().provider, Provider::Compatible);
        let err = c.endpoint_for("gpt-5").err().unwrap().to_string();
        assert!(err.contains("OPENAI_API_KEY"), "{err}");
        // A custom compatible endpoint takes models whose own provider has no key.
        let c = Client::new(vec![ep(Provider::Compatible, "http://localhost:4000/v1")]).unwrap();
        assert_eq!(c.endpoint_for("claude-opus-5-5").unwrap().provider, Provider::Compatible);
        assert!(Client::new(vec![]).is_err());
    }

    #[test]
    fn merges_streamed_tool_call_fragments() {
        let mut calls = Vec::new();
        merge_tool_call(
            &mut calls,
            &json!({"index": 0, "id": "a", "function": {"name": "read_file", "arguments": "{\"pa"}}),
        );
        merge_tool_call(&mut calls, &json!({"index": 0, "function": {"arguments": "th\": \"x\"}"}}));
        merge_tool_call(
            &mut calls,
            &json!({"index": 1, "id": "b", "function": {"name": "bash", "arguments": {"command": "ls"}}}),
        );
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].arguments, "{\"path\": \"x\"}");
        assert_eq!(calls[1].name, "bash");
        assert!(calls[1].arguments.contains("ls"));
    }
}
