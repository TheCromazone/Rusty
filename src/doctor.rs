//! `rusty --doctor` and `/doctor`: which providers are set up, whether each
//! one answers, and where the current model will go. Key values are never
//! shown, only the names of the variables that hold them.

use std::time::{Duration, Instant};

use crate::config::Provider;
use crate::llm::Client;
use crate::ui;

/// What one provider looks like from here.
pub struct Row {
    pub provider: Provider,
    pub host: String,
    /// Names of the variables that hold a key, in rotation order.
    pub vars: Vec<String>,
    /// None when not checked (no key, or offline).
    pub probe: Option<Result<(usize, Duration), String>>,
    /// Whether the probe listed `model` (when the model is this provider's).
    pub lists_model: Option<bool>,
}

/// Looks at every provider; with `live`, asks each configured one for its
/// model list once (no retries).
pub fn rows(client: Option<&Client>, model: &str, live: bool) -> Vec<Row> {
    let serving = client.and_then(|c| c.endpoint_for(model).ok()).map(|e| (e.provider, e.base_url.clone()));
    Provider::ALL
        .into_iter()
        .map(|p| {
            let ep = client.and_then(|c| c.endpoints().iter().find(|e| e.provider == p));
            // Codex has no key variables: the sign-in shows as the account it belongs to.
            let vars: Vec<String> = match p {
                Provider::Codex => {
                    ep.map(|e| vec![e.account().unwrap_or_else(|| "chatgpt sign-in".into())]).unwrap_or_default()
                }
                p => {
                    p.key_vars().into_iter().filter(|v| std::env::var(v).is_ok_and(|k| !k.trim().is_empty())).collect()
                }
            };
            let base = ep.map(|e| e.base_url.clone()).unwrap_or_else(|| p.base_url());
            let mut lists_model = None;
            let probe = match (client, ep) {
                (Some(c), Some(ep)) if live && p != Provider::Codex => {
                    let started = Instant::now();
                    Some(match c.models_of(ep, crate::llm::Retry::Once) {
                        Ok(ids) => {
                            // Codex publishes no catalog: its one "model" is just the configured default.
                            if p != Provider::Codex && serving.as_ref().is_some_and(|(sp, sb)| *sp == p && *sb == base)
                            {
                                lists_model = Some(ids.iter().any(|id| id == model));
                            }
                            Ok((ids.len(), started.elapsed()))
                        }
                        Err(e) => Err(short_error(&e.to_string())),
                    })
                }
                _ => None,
            };
            Row { provider: p, host: host(&base), vars, probe, lists_model }
        })
        .collect()
}

/// Cuts to `max` characters with an ellipsis.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn keys_label(vars: &[String]) -> String {
    match vars.len() {
        0 => String::new(),
        1 => vars[0].clone(),
        n => format!("{} +{}", vars[0], n - 1),
    }
}

fn host(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    rest.trim_end_matches('/').to_string()
}

fn short_error(e: &str) -> String {
    let first = e.lines().next().unwrap_or(e);
    let first = first.strip_prefix("giving up after ").map(|r| r.split_once(": ").map_or(r, |x| x.1)).unwrap_or(first);
    ui::truncate(first, 90).to_string()
}

/// The report as printed, and whether the current model has somewhere to go.
pub fn render(rows: &[Row], client: Option<&Client>, model: &str) -> (String, bool) {
    let mut out = String::new();
    // One row per provider on a wide terminal; two lines each on a narrow one.
    let narrow = ui::width() < 96;
    let room = ui::width().saturating_sub(8).max(20);
    let host_w = rows.iter().map(|r| r.host.chars().count()).max().unwrap_or(20).min(36);
    let keys_w = rows.iter().map(|r| keys_label(&r.vars).chars().count()).max().unwrap_or(0);
    out.push_str(&format!("\n  {}\n", ui::gradient("▓▒░ rusty doctor")));
    for r in rows {
        let name = format!("{:<11}", r.provider.name());
        if r.vars.is_empty() {
            let setup = r.provider.setup_hint();
            let hint = if narrow { setup } else { format!("not set up · {setup}") };
            out.push_str(&format!("  {} {} {}\n", ui::dim("○"), ui::dim(&name), ui::dim(&hint)));
            continue;
        }
        let (mark, status) = match &r.probe {
            None => (ui::info("•"), ui::dim("not checked")),
            Some(Ok((n, t))) => {
                (ui::ok("✓"), format!("{n} model{} · {} ms", if *n == 1 { "" } else { "s" }, t.as_millis()))
            }
            Some(Err(e)) => (ui::err("✗"), ui::err(if narrow { ui::truncate(e, room) } else { e })),
        };
        let host = ui::truncate(&r.host, if narrow { room } else { 36 });
        let keys = keys_label(&r.vars);
        if narrow {
            out.push_str(&format!("  {mark} {} {status}\n", ui::bold(&name)));
            out.push_str(&format!("    {}\n", ui::dim(&clip(&format!("{keys} · {host}"), room))));
        } else {
            out.push_str(&format!(
                "  {mark} {} {}  {}  {status}\n",
                ui::bold(&name),
                ui::dim(&format!("{host:<host_w$}")),
                ui::dim(&format!("{keys:<keys_w$}"))
            ));
        }
    }
    let route = client.map(|c| c.endpoint_for(model));
    let ok = match route {
        Some(Ok(ep)) => {
            let listed = rows.iter().find_map(|r| r.lists_model);
            let note = match listed {
                Some(true) => format!(" {} {}", ui::dim("·"), ui::ok("listed")),
                Some(false) => format!(" {} {}", ui::dim("·"), ui::warn("not in its model list; check the id")),
                None => String::new(),
            };
            let gap = if narrow { "\n   " } else { "" };
            out.push_str(&format!(
                "\n  model {}{gap} {} {}{note}\n",
                ui::info(&clip(model, room)),
                ui::dim("→"),
                ep.provider.name()
            ));
            true
        }
        Some(Err(e)) => {
            out.push_str(&format!("\n  model {} {} {}\n", ui::info(model), ui::dim("→"), ui::err(&e.to_string())));
            false
        }
        None => {
            out.push_str(&format!(
                "\n  {}\n",
                ui::warn(
                    "no keys yet: set NVIDIA_API_KEY, ANTHROPIC_API_KEY or OPENAI_API_KEY in ~/.config/rusty/.env"
                )
            ));
            false
        }
    };
    let probes_ok = rows.iter().filter_map(|r| r.probe.as_ref()).all(Result::is_ok);
    (out, ok && probes_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_one_short_line() {
        assert_eq!(
            short_error("giving up after 1 attempts: HTTP 401 Unauthorized: bad key\nmore"),
            "HTTP 401 Unauthorized: bad key"
        );
        assert_eq!(host("https://api.anthropic.com/"), "api.anthropic.com");
    }

    #[test]
    fn an_unset_provider_says_which_variable_to_add() {
        let rows = vec![Row {
            provider: Provider::OpenAi,
            host: "api.openai.com/v1".into(),
            vars: vec![],
            probe: None,
            lists_model: None,
        }];
        let (text, ok) = render(&rows, None, "gpt-5");
        assert!(text.contains("add OPENAI_API_KEY") && text.contains("no keys yet"), "{text}");
        assert_eq!(clip("abcdef", 4), "abc…");
        assert_eq!(clip("abc", 4), "abc");
        assert_eq!(keys_label(&["A".into(), "A_2".into(), "A_3".into()]), "A +2");
        assert!(!ok);
    }
}
