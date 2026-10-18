//! Optional native-app tooling; never linked into the shipped CLI.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::Command;

pub fn protocol() -> Result<()> {
    let status = Command::new("cargo")
        .args(["test", "--bin", "rusty", "protocol::tests::generate_artifacts", "--", "--ignored", "--exact"])
        .current_dir(crate::root())
        .status()?;
    if !status.success() {
        bail!("protocol artifact generation failed");
    }
    Ok(())
}
// Tokenize enough Rust to distinguish comments/literals from item delimiters.
// Unknown cfg expressions remain in the audited source: only exact cfg(test)
// items are excluded, so feature-gated production embeds are still checked.
fn source_tokens(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < b.len() && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let start = i;
        let raw_start = if b[i] == b'b' { i + 1 } else { i };
        if raw_start < b.len() && b[raw_start] == b'r' {
            let mut quote = raw_start + 1;
            while quote < b.len() && b[quote] == b'#' {
                quote += 1;
            }
            if quote < b.len() && b[quote] == b'"' {
                let end = format!("\"{}", "#".repeat(quote - raw_start - 1));
                i = s[quote + 1..].find(&end).map_or(b.len(), |n| quote + 1 + n + end.len());
                out.push(&s[start..i]);
                continue;
            }
        }
        if b[i] == b'"' || (b[i] == b'b' && b.get(i + 1) == Some(&b'"')) {
            if b[i] == b'b' {
                i += 1;
            }
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else if b[i] == b'\'' && s[i + 1..].find('\'').is_some_and(|n| n <= 10) {
            // Character literals may contain braces; lifetime names have no
            // nearby closing apostrophe and remain ordinary punctuation.
            i += 1;
            if b.get(i) == Some(&b'\\') {
                i += 1;
                if b.get(i) == Some(&b'u') {
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                } else {
                    i = (i + 1).min(b.len());
                }
            } else if i < b.len() {
                i += s[i..].chars().next().unwrap().len_utf8();
            }
            if b.get(i) == Some(&b'\'') {
                i += 1;
            }
        } else if b[i].is_ascii_alphanumeric() || b[i] == b'_' {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
        } else {
            i += s[i..].chars().next().unwrap().len_utf8();
        }
        out.push(&s[start..i]);
    }
    out
}
fn production_tokens(s: &str) -> Vec<&str> {
    let tokens = source_tokens(s);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i..].starts_with(&["#", "[", "cfg", "(", "test", ")", "]"]) {
            i += 7;
            let mut depth = 0usize;
            let mut body = false;
            while i < tokens.len() {
                let t = tokens[i];
                i += 1;
                match t {
                    "(" | "[" => depth += 1,
                    ")" | "]" => depth = depth.saturating_sub(1),
                    "{" => {
                        if depth == 0 {
                            body = true;
                        }
                        depth += 1;
                    }
                    "}" => {
                        depth = depth.saturating_sub(1);
                        if body && depth == 0 {
                            break;
                        }
                    }
                    ";" if depth == 0 => break,
                    _ => {}
                }
            }
        } else {
            out.push(tokens[i]);
            i += 1;
        }
    }
    out
}
fn gui_literal(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    token.contains('"')
        && ["apps/", "apps\\", ".swift", ".html", ".css", ".xib", ".storyboard", ".xcassets"]
            .iter()
            .any(|needle| lower.contains(needle))
}
fn audit_source(source: &str, build_script: bool) -> Result<()> {
    let tokens = production_tokens(source);
    for (i, token) in tokens.iter().enumerate() {
        if build_script && gui_literal(token) {
            bail!("GUI resource referenced by build script");
        }
        if matches!(*token, "include_str" | "include_bytes") && tokens.get(i + 1) == Some(&"!") {
            // Reject computed include paths: concat!/env! could hide a GUI asset.
            let argument = tokens.get(i + 3).context("missing include argument")?;
            if !argument.contains('"') || gui_literal(argument) {
                bail!("GUI or computed resource embed in production source");
            }
        }
    }
    Ok(())
}
fn audit_tree(path: &Path) -> Result<()> {
    for entry in std::fs::read_dir(path)? {
        let p = entry?.path();
        if p.is_dir() {
            audit_tree(&p)?;
        } else if p.extension().is_some_and(|x| x == "rs") {
            audit_source(&std::fs::read_to_string(&p)?, false)
                .with_context(|| format!("source gate: {}", p.display()))?;
        }
    }
    Ok(())
}
fn normal_dependencies(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("cargo")
        .args(["tree", "--locked", "--offline", "-p", "rusty", "-e", "normal", "--prefix", "none", "--format", "{p}"])
        .current_dir(root)
        .output()?;
    if !output.status.success() {
        bail!("normal dependency inspection failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    let stdout = String::from_utf8(output.stdout)?;
    let mut packages: Vec<String> = stdout
        .lines()
        .skip(1)
        .map(|line| line.trim_end_matches(" (*)").replace(&root.display().to_string(), "<SOURCE>"))
        .collect();
    packages.sort();
    packages.dedup();
    Ok(packages)
}
fn compare_normal_dependencies(before: &[String], after: &[String]) -> Result<()> {
    if before != after {
        bail!("production normal dependencies changed: before={before:?} after={after:?}");
    }
    Ok(())
}
pub fn cli_gate(args: &[String]) -> Result<()> {
    let candidate =
        args.first().context("usage: cargo xtask cli-gate CANDIDATE [BASELINE_BINARY] [BASELINE_SOURCE]")?;
    let root = crate::root();
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    for banned in ["tauri", "wry", "swift-rs", "webkit", "apps/macos"] {
        if manifest.to_ascii_lowercase().contains(banned) {
            bail!("GUI dependency/path in CLI manifest: {banned}");
        }
    }
    // The default build.rs is audited below; reject custom build script paths
    // rather than silently omitting their inputs from the audit.
    for line in manifest.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once('=') {
            if key.trim() == "build" && !matches!(value.trim(), "false" | "true" | "\"build.rs\"") {
                bail!("custom build script requires explicit source audit");
            }
        }
    }
    audit_tree(&root.join("src"))?;
    let build_script = root.join("build.rs");
    if build_script.exists() {
        audit_source(&std::fs::read_to_string(build_script)?, true)?;
    }
    let dependencies = normal_dependencies(&root)?;
    let size = std::fs::metadata(candidate)?.len();
    let mut report = json!({"gui_dependency_manifest_check":"passed","embedded_gui_source_check":"passed","build_script_check":"passed","candidate_bytes":size,"size_delta_bytes":null,"normal_dependencies":dependencies,"normal_dependency_comparison":"unqualified: baseline source not supplied"});
    if let Some(baseline_source) = args.get(2) {
        let before = normal_dependencies(&std::fs::canonicalize(baseline_source)?)?;
        compare_normal_dependencies(&before, &dependencies)?;
        report["normal_dependency_comparison"] = json!("passed");
    }
    if let Some(baseline) = args.get(1) {
        let old = std::fs::metadata(baseline)?.len();
        report["baseline_bytes"] = json!(old);
        report["size_delta_bytes"] = json!(i128::from(size) - i128::from(old));
        if cfg!(target_os = "macos") {
            fn libs(path: &str) -> Result<Vec<String>> {
                let o = Command::new("otool").args(["-L", path]).output()?;
                if !o.status.success() {
                    bail!("otool failed");
                }
                Ok(String::from_utf8(o.stdout)?.lines().skip(1).map(|l| l.trim().to_string()).collect())
            }
            let before = libs(baseline)?;
            let after = libs(candidate)?;
            report["linked_libraries"] = json!(after);
            if before != after {
                bail!("CLI linked libraries changed: before={before:?} after={after:?}");
            }
        }
    }
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
/// Bounded loopback-only scripted provider for native UI integration tests.
pub fn fixture_provider(args: &[String]) -> Result<()> {
    let file = args.first().context("usage: cargo xtask fixture-provider REPLIES.json")?;
    let replies: Vec<Value> = serde_json::from_slice(&std::fs::read(file)?)?;
    if replies.is_empty() || replies.len() > 128 {
        bail!("fixture requires 1..128 replies");
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    println!("{}", json!({"base_url":format!("http://{}/v1",listener.local_addr()?)}));
    std::io::stdout().flush()?;
    listener.set_nonblocking(true)?;
    for reply in replies {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
        let mut stream = loop {
            match listener.accept() {
                Ok((s, _)) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        bail!("fixture provider timed out");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return Err(e.into()),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut length = 0;
        let mut header_bytes = 0;
        loop {
            let mut line = String::new();
            let n = std::io::Read::by_ref(&mut reader).take(8193).read_line(&mut line)?;
            header_bytes += n;
            if n == 0 || header_bytes > 16384 {
                bail!("invalid fixture HTTP headers");
            }
            if line == "\r\n" {
                break;
            }
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse::<usize>()?;
                }
            }
        }
        if length > 1024 * 1024 {
            bail!("fixture request too large");
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let _: Value = serde_json::from_slice(&body)?;
        let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len())?;
    }
    Ok(())
}

#[cfg(test)]
mod cli_gate_tests {
    use super::*;
    #[test]
    fn changed_production_dependencies_fail_comparison() {
        let baseline = vec!["anyhow v1.0.0".to_owned()];
        assert!(compare_normal_dependencies(&baseline, &baseline).is_ok());
        assert!(compare_normal_dependencies(&baseline, &["anyhow v1.0.0".to_owned(), "wry v1.0.0".to_owned()]).is_err());
        assert!(compare_normal_dependencies(&baseline, &["anyhow v2.0.0".to_owned()]).is_err());
    }
    #[test]
    fn rejects_gui_embeds_and_computed_paths() {
        for source in [
            r#"const A: &str = include_str!("../apps/macos/main.swift");"#,
            r#"const A: &[u8] = include_bytes!("../assets/page.html");"#,
            r#"const A: &str = include_str!(concat!(env!("OUT_DIR"), "/gui"));"#,
            r##"const A: &str = include_str!(r#"../apps/macos/main.swift"#);"##,
            r#"#[cfg(feature = "desktop")] fn ui() { include_str!("view.swift"); }"#,
        ] {
            assert!(audit_source(source, false).is_err(), "{source}");
        }
    }
    #[test]
    fn ignores_only_exact_test_items_and_comments() {
        let source = r##"
            // include_str!("apps/gui.swift")
            /* nested /* include_bytes!("view.html") */ comment */
            #[cfg(test)] fn generator() {
                let brace = '}'; let text = "} fake body";
                include_str!("apps/macos/Test.swift");
            }
            #[cfg(test)] mod tests { fn test() { include_bytes!("view.html"); } }
            const HASH: &str = include_str!("protocol_schema_hash.txt");
        "##;
        assert!(audit_source(source, false).is_ok());
        assert!(
            audit_source(&format!("{source}\nconst LEAK: &str = include_str!(\"apps/live.swift\");"), false).is_err()
        );
        assert!(audit_source(
            r#"#[cfg(any(test, feature = "gui"))] fn leaked() { include_str!("view.swift"); }"#,
            false
        )
        .is_err());
    }
    #[test]
    fn build_scripts_cannot_copy_or_embed_gui_resources() {
        assert!(audit_source(r#"fn main() { std::fs::copy("apps/macos/view.swift", "out").unwrap(); }"#, true).is_err());
        assert!(audit_source(r#"fn main() { include_str!("view.html"); }"#, true).is_err());
        assert!(audit_source(r#"fn main() { println!("cargo:rerun-if-changed=src/schema.txt"); }"#, true).is_ok());
    }
}
