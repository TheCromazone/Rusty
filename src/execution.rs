//! Execution profiles control inference and review, independently of permissions.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Careful,
    #[default]
    Standard,
    Vibe,
}

impl ExecutionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "careful" => Some(Self::Careful),
            "standard" => Some(Self::Standard),
            "vibe" => Some(Self::Vibe),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Careful => "careful",
            Self::Standard => "standard",
            Self::Vibe => "vibe",
        }
    }

    pub fn model(self) -> Option<String> {
        std::env::var(format!("RUSTY_{}_MODEL", self.name().to_uppercase())).ok().filter(|s| !s.trim().is_empty())
    }

    pub fn max_tokens(self) -> u32 {
        match self {
            Self::Careful => 32768,
            Self::Standard => 16384,
            Self::Vibe => 8192,
        }
    }

    /// Careful mode has one read-only checker review the finished work.
    pub fn checker(self) -> &'static str {
        if self == Self::Careful {
            "one read-only check"
        } else {
            "off"
        }
    }

    /// Vibe keeps delegation small unless the user picked a swarm size.
    pub const VIBE_WORKERS: usize = 3;

    pub fn instructions(self) -> &'static str {
        match self {
            Self::Careful => "Execution: careful. Spend reasoning on assumptions, failure modes and side effects. Before critical changes inspect the target state and describe a bounded plan with recovery steps. Make small changes, verify the result, then recheck the diff and task constraints. Use actual tool evidence; a review is not proof of production safety. Reconcile uncertain external writes before retrying. Never invent checks or rerun side-effecting actions as verification.",
            Self::Standard => "Execution: standard. Use a focused plan and normal reasoning. Verify the changed behavior with relevant tests or a direct run, and report remaining uncertainty.",
            Self::Vibe => "Execution: vibe. Optimize for fast, small, reversible iterations. Keep planning brief, use focused smoke checks, and avoid redundant reviews. Delegate independent research to a small number of read-only workers when useful; you own integration and verification. Do not skip the relevant check or claim untested behavior works. For critical production, auth, payment or data changes recommend careful mode before acting; execution mode never grants permission.",
        }
    }

    /// The documented NVIDIA Super controls are opt-in for a known endpoint/model.
    /// Unknown compatible endpoints keep their usual request schema, and never
    /// get an output cap above the 16k every model has handled so far.
    pub fn apply_inference(self, body: &mut Value, base_url: &str, model: &str) {
        let known = base_url == crate::config::DEFAULT_BASE_URL && model == crate::config::DEFAULT_MODEL;
        body["max_tokens"] = json!(if known { self.max_tokens() } else { self.max_tokens().min(16384) });
        if known {
            match self {
                Self::Careful => {
                    body["reasoning_effort"] = json!("high");
                    body["reasoning_budget"] = json!(24576);
                }
                Self::Standard => {}
                Self::Vibe => {
                    body["reasoning_effort"] = json!("low");
                    body["reasoning_budget"] = json!(2048);
                }
            }
        }
    }
}

/// Picks a starting profile for a request when the user hasn't chosen one,
/// with a short reason. Only clear signals move it off standard.
pub fn pick(request: &str) -> (ExecutionMode, String) {
    let lower = request.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let word = |w: &[&str]| words.iter().any(|x| w.contains(x));
    let stem = |s: &[&str]| words.iter().any(|x| s.iter().any(|p| x.starts_with(p)));
    let phrase = |p: &[&str]| p.iter().any(|p| lower.contains(p));
    let careful: [(&str, bool); 9] = [
        ("production", word(&["prod", "production"]) || live_means_production(&words)),
        ("a migration", stem(&["migrat", "schema", "backfill"])),
        ("a deploy", stem(&["deploy", "rollout"]) || word(&["release"])),
        (
            "data",
            word(&["database", "db", "sql", "postgres", "mysql", "redis", "mongo"])
                || phrase(&["data loss", "user data"]),
        ),
        ("money", stem(&["payment", "billing", "invoice", "refund", "charge", "stripe", "ledger"])),
        (
            "security",
            stem(&[
                "auth",
                "security",
                "vulnerab",
                "secret",
                "credential",
                "encrypt",
                "permission",
                "password",
                "token",
            ]),
        ),
        ("infrastructure", stem(&["terraform", "kubernetes", "kubectl", "helm", "infra", "k8s", "dns", "iam"])),
        ("an incident", stem(&["incident", "outage", "rollback", "hotfix", "postmortem"])),
        (
            "you asked for care",
            stem(&["careful", "critical", "irreversib"]) || phrase(&["don't break", "do not break"]),
        ),
    ];
    let hits: Vec<&str> = careful.iter().filter(|(_, hit)| *hit).map(|(why, _)| *why).collect();
    if !hits.is_empty() {
        let shown = hits.iter().take(2).copied().collect::<Vec<_>>().join(" and ");
        return (ExecutionMode::Careful, format!("mentions {shown}"));
    }
    if stem(&["prototyp", "sketch", "spike", "throwaway", "mockup", "playground"])
        || word(&["quick", "quickly", "rough", "poc", "toy", "scratch", "demo"])
        || phrase(&["mock up", "proof of concept", "just try"])
    {
        return (ExecutionMode::Vibe, "sounds like a quick prototype".into());
    }
    (ExecutionMode::Standard, "nothing unusual".into())
}

/// "live" on its own is everyday wording: a live test, live typing, live
/// reload. It means production next to something that runs for real (the
/// live site, live traffic) or as where the work goes (go live, deploy to
/// live).
fn live_means_production(words: &[&str]) -> bool {
    const TARGETS: &[&str] = &[
        "site",
        "website",
        "server",
        "servers",
        "environment",
        "env",
        "system",
        "systems",
        "database",
        "db",
        "data",
        "traffic",
        "users",
        "customers",
        "api",
        "app",
        "service",
        "services",
        "cluster",
        "instance",
        "deployment",
        "keys",
        "key",
        "mode",
        "account",
        "store",
        "endpoint",
        "payments",
        "orders",
        "infrastructure",
        "infra",
        "stack",
        "version",
    ];
    const INTO: &[&str] = &["go", "goes", "going", "went", "push", "pushed", "pushing", "to", "on", "in", "against"];
    const EVERYDAY: &[&str] = &[
        "reload",
        "preview",
        "stream",
        "streaming",
        "test",
        "tests",
        "testing",
        "login",
        "typing",
        "session",
        "sessions",
        "demo",
        "coding",
        "share",
        "chat",
        "update",
        "updates",
        "search",
        "edit",
        "editing",
        "view",
    ];
    words.iter().enumerate().any(|(i, w)| {
        let next = words.get(i + 1).copied();
        *w == "live"
            && (next.is_some_and(|n| TARGETS.contains(&n))
                || i > 0 && INTO.contains(&words[i - 1]) && !next.is_some_and(|n| EVERYDAY.contains(&n)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_controls_are_scoped_to_the_supported_provider() {
        let mut body = json!({});
        ExecutionMode::Careful.apply_inference(
            &mut body,
            crate::config::DEFAULT_BASE_URL,
            crate::config::DEFAULT_MODEL,
        );
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["reasoning_budget"], 24576);
        let mut body = json!({});
        ExecutionMode::Vibe.apply_inference(&mut body, "http://localhost:11434/v1", "other-model");
        assert_eq!(body["max_tokens"], 8192);
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("reasoning_budget").is_none());
        let mut body = json!({});
        ExecutionMode::Careful.apply_inference(&mut body, "http://localhost:11434/v1", "other-model");
        assert_eq!(body["max_tokens"], 16384, "unknown models keep the old cap");
        ExecutionMode::Vibe.apply_inference(&mut body, crate::config::DEFAULT_BASE_URL, crate::config::DEFAULT_MODEL);
        assert_eq!(body["reasoning_effort"], "low");
    }

    #[test]
    fn picks_a_profile_from_clear_signals_only() {
        let p = |s: &str| pick(s).0;
        assert_eq!(p("add a migration for the orders table"), ExecutionMode::Careful);
        assert_eq!(p("deploy the api to prod"), ExecutionMode::Careful);
        assert_eq!(p("fix the stripe refund webhook"), ExecutionMode::Careful);
        assert_eq!(p("rotate the terraform state bucket"), ExecutionMode::Careful);
        assert_eq!(p("quick prototype of a settings page"), ExecutionMode::Vibe);
        assert_eq!(p("sketch a landing page"), ExecutionMode::Vibe);
        assert_eq!(p("fix the failing parser test"), ExecutionMode::Standard);
        assert_eq!(p("why is this slow?"), ExecutionMode::Standard);
        assert_eq!(p("add live reload to the dev server"), ExecutionMode::Standard);
        // Consequential beats quick: a quick prod fix is still prod.
        assert_eq!(p("quick hotfix in production"), ExecutionMode::Careful);
        assert_eq!(pick("deploy the db migration").1, "mentions a migration and a deploy");
    }

    #[test]
    fn live_alone_is_not_production() {
        let p = |s: &str| pick(s);
        for everyday in [
            "Live login and typing smoke test. Do not use tools or modify files.",
            "run a live test of the parser",
            "add live streaming to the chat view",
            "the live preview is blank",
            "set up a live coding session",
            "how do I add a button to live stream?",
        ] {
            assert_eq!(p(everyday), (ExecutionMode::Standard, "nothing unusual".into()), "{everyday}");
        }
        for target in [
            "check the live site after the change",
            "why is live traffic dropping?",
            "rotate the live api keys",
            "the cart goes live tomorrow, fix the totals",
            "push the hotfix to live",
            "the bug only shows up on live",
        ] {
            assert_eq!(p(target).0, ExecutionMode::Careful, "{target}");
            assert!(p(target).1.contains("production"), "{target}: {}", p(target).1);
        }
        // An explicit prod is production even beside an everyday "live".
        assert_eq!(p("fix live reload on the prod server").1, "mentions production");
    }
}
