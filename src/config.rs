//! Environment loading: API keys, base URL and defaults.

use std::path::PathBuf;

pub const DEFAULT_BASE_URL: &str = "https://integrate.api.nvidia.com/v1";
pub const DEFAULT_MODEL: &str = "nvidia/nemotron-3-super-120b-a12b";
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const OPENAI_MODEL: &str = "gpt-5";
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_MODEL: &str = "claude-opus-5-5";

/// Who serves a model. Each has its own endpoint and keys; rusty picks one per
/// request from the model id, so one session can mix them (say, Claude as the
/// lead and a small open model for swarm workers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// Any OpenAI-compatible endpoint: NVIDIA by default, or `RUSTY_BASE_URL`.
    Compatible,
    OpenAi,
    Anthropic,
    /// The Codex backend, signed in with the ChatGPT account from `codex login`.
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 4] = [Provider::Compatible, Provider::Anthropic, Provider::OpenAi, Provider::Codex];

    pub fn name(self) -> &'static str {
        match self {
            Self::Compatible => "compatible",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Codex => "codex",
        }
    }

    /// What to do to set this provider up, for error messages and the doctor.
    pub fn setup_hint(self) -> String {
        match self {
            Self::Codex => "run `codex login`".into(),
            p => {
                let vars = p.key_vars();
                format!("add {}", vars.iter().find(|v| !v.starts_with("RUSTY_")).unwrap_or(&vars[0]))
            }
        }
    }

    /// The provider a model id belongs to. Bare `claude-*` ids are Anthropic's
    /// and bare `gpt-*` / `o1`-style ids are OpenAI's; anything with a slash
    /// (`openai/gpt-oss-20b` on NVIDIA, `anthropic/claude-…` on a router) or
    /// any other name goes to the compatible endpoint. `RUSTY_PROVIDER`
    /// overrides the guess. An OpenAI model goes to the Codex sign-in instead
    /// when there is no OpenAI key (see `Client::endpoint_for`).
    pub fn for_model(model: &str) -> Provider {
        if let Some(p) = env_nonempty("RUSTY_PROVIDER").and_then(|v| Provider::parse(&v)) {
            return p;
        }
        let m = model.trim().to_ascii_lowercase();
        if m.contains('/') {
            return Provider::Compatible;
        }
        if m.starts_with("claude-") {
            return Provider::Anthropic;
        }
        let o_series = m.len() >= 2 && m.starts_with('o') && m.as_bytes()[1].is_ascii_digit();
        if m.starts_with("gpt-") || m.starts_with("chatgpt-") || m.starts_with("codex-") || o_series {
            return Provider::OpenAi;
        }
        Provider::Compatible
    }

    pub fn parse(s: &str) -> Option<Provider> {
        match s.trim().to_ascii_lowercase().as_str() {
            "compatible" | "nvidia" | "openai-compatible" => Some(Self::Compatible),
            "openai" => Some(Self::OpenAi),
            "anthropic" | "claude" => Some(Self::Anthropic),
            "codex" | "chatgpt" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Environment variables that hold this provider's keys, in rotation order.
    pub fn key_vars(self) -> Vec<String> {
        let (first, base): (&[&str], &str) = match self {
            Self::Compatible => (&["RUSTY_API_KEY", "NVIDIA_API_KEY"], "NVIDIA_API_KEY"),
            Self::OpenAi => (&["OPENAI_API_KEY"], "OPENAI_API_KEY"),
            Self::Anthropic => (&["ANTHROPIC_API_KEY"], "ANTHROPIC_API_KEY"),
            // Signed in, not keyed: the tokens live in ~/.codex/auth.json.
            Self::Codex => return Vec::new(),
        };
        let mut names: Vec<String> = first.iter().map(|s| s.to_string()).collect();
        names.extend((2..=9).map(|i| format!("{base}_{i}")));
        names
    }

    pub fn keys(self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for name in self.key_vars() {
            if let Ok(k) = std::env::var(&name) {
                let k = k.trim().to_string();
                if !k.is_empty() && !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        keys
    }

    pub fn base_url(self) -> String {
        let url = match self {
            Self::Compatible => return base_url(),
            Self::OpenAi => env_nonempty("OPENAI_BASE_URL").unwrap_or_else(|| OPENAI_BASE_URL.to_string()),
            Self::Anthropic => env_nonempty("ANTHROPIC_BASE_URL").unwrap_or_else(|| ANTHROPIC_BASE_URL.to_string()),
            Self::Codex => env_nonempty("RUSTY_CODEX_BASE_URL").unwrap_or_else(|| crate::codex::BASE_URL.to_string()),
        };
        url.trim_end_matches('/').to_string()
    }

    pub fn default_model(self) -> String {
        match self {
            Self::Compatible => DEFAULT_MODEL.into(),
            Self::OpenAi => OPENAI_MODEL.into(),
            Self::Anthropic => ANTHROPIC_MODEL.into(),
            // Whatever Codex itself is set to use, so both tools agree.
            Self::Codex => crate::codex::default_model(),
        }
    }

    /// Whether this provider can be used from here: a key in the
    /// environment, or for Codex a saved sign-in.
    pub fn configured(self) -> bool {
        match self {
            Self::Codex => crate::codex::Auth::load().is_some(),
            p => !p.keys().is_empty(),
        }
    }
}

/// The model to use when none was chosen: the default of the first provider
/// that is set up, in the order compatible (NVIDIA), Anthropic, OpenAI, then
/// the Codex sign-in, so an existing NVIDIA setup behaves exactly as before.
pub fn default_model() -> String {
    if let Some(p) = env_nonempty("RUSTY_PROVIDER").and_then(|v| Provider::parse(&v)) {
        return p.default_model();
    }
    Provider::ALL.into_iter().find(|p| p.configured()).unwrap_or(Provider::Compatible).default_model()
}

/// Loads `.env` files without overriding variables already set in the shell.
/// Order: `~/.config/rusty/.env`, then the `.env` next to this crate's sources
/// (so `cargo install --path .` works with the repo's own `.env`).
pub fn load_env() {
    if std::env::var_os("RUSTY_NO_DOTENV").is_some() {
        return;
    }
    if let Some(dir) = config_dir() {
        let _ = dotenvy::from_path(dir.join(".env"));
    }
    let _ = dotenvy::from_path(concat!(env!("CARGO_MANIFEST_DIR"), "/.env"));
}

/// `RUSTY_HOME` overrides the default `~/.config/rusty` (handy for tests).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("RUSTY_HOME") {
        return Some(PathBuf::from(h));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("rusty"))
}

pub fn base_url() -> String {
    env_nonempty("RUSTY_BASE_URL")
        .or_else(|| env_nonempty("NVIDIA_API_BASE"))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// A variable's value, treating set-but-empty (`RUSTY_BASE_URL=`) as unset.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// How the agent may delegate work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentsMode {
    /// No delegation.
    Off,
    /// One read-only subagent at a time (`task` tool).
    Sub,
    /// Many read-only workers in parallel (`swarm` tool).
    Swarm,
    /// Both available; the model picks the lightest option that fits.
    Auto,
}

impl AgentsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "sub" | "subagents" | "on" => Some(Self::Sub),
            "swarm" => Some(Self::Swarm),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Sub => "sub",
            Self::Swarm => "swarm",
            Self::Auto => "auto",
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentsConfig {
    pub mode: AgentsMode,
    /// Model for subagents; defaults to the main model.
    pub sub_model: Option<String>,
    /// Models handed out round-robin to swarm workers; empty = sub_model.
    pub swarm_models: Vec<String>,
    /// Most workers one swarm call may start.
    pub swarm_max: usize,
    /// Temperature spread across workers (0 = all alike, 1 = very varied).
    pub spread: f32,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self { mode: AgentsMode::Off, sub_model: None, swarm_models: Vec::new(), swarm_max: 8, spread: 0.4 }
    }
}

/// Preferences that persist across sessions in ~/.config/rusty/settings.json.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Tool location for future sessions; a bridge is required for daytona.
    pub tools: String,
    pub theme: String,
    pub font: String,
    pub view: String,
    pub tips: bool,
    pub agents: AgentsConfig,
    /// None means rusty picks per request.
    pub execution_mode: Option<crate::execution::ExecutionMode>,
    pub delegation_override: bool,
    /// When history is compacted (see `context::Compaction`); unset picks by
    /// window size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
    /// Compaction thresholds never sit above this many context tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_cap_tokens: Option<usize>,
    /// The model that writes compaction summaries; unset uses the session's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction_model: Option<String>,
    /// Context windows by model id, for endpoints that don't publish one.
    pub context_windows: std::collections::BTreeMap<String, usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tools: "local".into(),
            theme: "rust".into(),
            font: "compact".into(),
            view: "default".into(),
            tips: true,
            agents: AgentsConfig::default(),
            execution_mode: None,
            delegation_override: false,
            compaction: None,
            history_cap_tokens: None,
            compaction_model: None,
            context_windows: Default::default(),
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        config_dir().map(|d| d.join("settings.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = Self::path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(value) = serde_json::to_value(self) {
                let _ = rusty::privacy::write_json(&p, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Provider;

    #[test]
    fn models_route_to_their_provider() {
        for (model, provider) in [
            ("claude-opus-5-5", Provider::Anthropic),
            ("claude-haiku-4-5", Provider::Anthropic),
            ("gpt-5", Provider::OpenAi),
            ("gpt-4.1-mini", Provider::OpenAi),
            ("o4-mini", Provider::OpenAi),
            ("openai/gpt-oss-20b", Provider::Compatible),
            ("anthropic/claude-opus-5-5", Provider::Compatible),
            ("nvidia/nemotron-3-super-120b-a12b", Provider::Compatible),
            ("z-ai/glm-5.3", Provider::Compatible),
            ("llama3.2", Provider::Compatible),
            ("orca-mini", Provider::Compatible),
        ] {
            assert_eq!(Provider::for_model(model), provider, "{model}");
        }
    }

    #[test]
    fn provider_names_parse() {
        assert_eq!(Provider::parse("Claude"), Some(Provider::Anthropic));
        assert_eq!(Provider::parse("nvidia"), Some(Provider::Compatible));
        assert_eq!(Provider::parse("openai"), Some(Provider::OpenAi));
        assert_eq!(Provider::parse("chatgpt"), Some(Provider::Codex));
        assert!(Provider::Codex.key_vars().is_empty());
        assert_eq!(Provider::Codex.setup_hint(), "run `codex login`");
        assert_eq!(Provider::OpenAi.setup_hint(), "add OPENAI_API_KEY");
        assert_eq!(Provider::parse("bedrock"), None);
        assert_eq!(Provider::OpenAi.key_vars()[1], "OPENAI_API_KEY_2");
    }
}
