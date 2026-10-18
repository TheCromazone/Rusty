//! Git and forge setup without a second credential store or SDK stack.
use crate::{
    backend::Backend,
    permissions::{Policy, Verdict},
};
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::time::Duration;

#[derive(Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    provider: Option<String>,
    remote: Option<String>,
}
#[derive(Debug, PartialEq)]
struct Repo {
    host: String,
    path: String,
}
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn load(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    serde_json::from_slice(&std::fs::read(path)?)
        .context("invalid integrations config; expected provider and remote only")
}
fn provider(name: &str) -> Result<&str> {
    ensure!(matches!(name, "github" | "gitlab" | "bitbucket"), "provider must be github, gitlab or bitbucket");
    Ok(name)
}
fn remote_url(url: &str) -> Result<Repo> {
    let url = url.trim();
    let (host, path) = if url.contains("://") {
        let u = reqwest::Url::parse(url).context("invalid Git remote URL")?;
        ensure!(
            matches!(u.scheme(), "https" | "ssh")
                && (u.scheme() != "https" || u.username().is_empty())
                && u.password().is_none()
                && u.query().is_none()
                && u.fragment().is_none(),
            "use an SSH or HTTPS remote without embedded credentials or query parameters"
        );
        (u.host_str().context("remote has no host")?.to_string(), u.path().trim_start_matches('/').to_string())
    } else {
        let (host, path) = url.split_once(':').context("forge requires an SSH or HTTPS remote, not a local path")?;
        (host.rsplit('@').next().unwrap_or(host).to_string(), path.to_string())
    };
    ensure!(
        !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
        "invalid forge hostname"
    );
    let path = path.trim_end_matches('/').strip_suffix(".git").unwrap_or(path.trim_end_matches('/')).to_string();
    let segments: Vec<_> = path.split('/').collect();
    ensure!(
        segments.len() >= 2
            && segments.iter().all(|p| !p.is_empty()
                && *p != "."
                && *p != ".."
                && !p.starts_with('-')
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "invalid forge repository path"
    );
    Ok(Repo { host: host.to_ascii_lowercase(), path })
}
fn infer(repo: &Repo, config: &Config) -> Result<String> {
    if let Some(name) = &config.provider {
        return Ok(provider(name)?.to_string());
    }
    Ok(match repo.host.as_str() {
        "github.com" => "github",
        "gitlab.com" => "gitlab",
        "bitbucket.org" => "bitbucket",
        _ => bail!(
            "unknown forge host {}; use /integrations configure github|gitlab <remote> for self-hosted Git",
            repo.host
        ),
    }
    .into())
}
fn allowed(backend: &Backend, policy: &Policy, cwd: &Path, command: &str) -> Result<()> {
    let verdict = backend.decide("bash", &json!({"command": command}), policy, cwd)?;
    ensure!(matches!(verdict, Verdict::Allow), "integration command refused by permissions: {verdict:?}");
    Ok(())
}
fn run(backend: &Backend, policy: &Policy, cwd: &Path, command: &str) -> Result<String> {
    allowed(backend, policy, cwd, command)?;
    let (status, stdout, stderr) = backend.shell(command, Duration::from_secs(15))?;
    if status != Some(Some(0)) {
        let message = rusty::privacy::redact(&stderr).0;
        bail!("integration command failed or timed out: {}", crate::ui::truncate(message.trim(), 300));
    }
    Ok(rusty::privacy::redact(&stdout).0)
}
fn repository(backend: &Backend, policy: &Policy, cwd: &Path, config: &Config) -> Result<Repo> {
    let remote = config.remote.as_deref().unwrap_or("origin");
    ensure!(
        !remote.starts_with('-') && remote.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b)),
        "invalid remote name"
    );
    let url = run(backend, policy, cwd, &format!("git remote get-url {}", quote(remote)))?;
    remote_url(&url)
}
fn setup(name: &str, repo: Option<&Repo>) -> Result<String> {
    provider(name)?;
    let host = repo.map(|r| r.host.as_str()).unwrap_or(match name {
        "github" => "github.com",
        "gitlab" => "gitlab.com",
        _ => "bitbucket.org",
    });
    Ok(match name {
        "github" => format!("GitHub uses your existing gh account. Install GitHub CLI, then run:\n  gh auth login --hostname {}\n  gh auth setup-git --hostname {}\nCheck it with /integrations check. Rusty does not store a GitHub token.", quote(host), quote(host)),
        "gitlab" => format!("GitLab uses your existing glab account. Install GitLab CLI, then run:\n  glab auth login --hostname {}\nUse your existing Git SSH/HTTPS credential helper. Check with /integrations check. Rusty does not store a GitLab token.", quote(host)),
        _ => "Bitbucket Cloud uses BITBUCKET_ACCESS_TOKEN (repository/project/workspace access token), or BITBUCKET_EMAIL with BITBUCKET_API_TOKEN. Set them in your shell or private ~/.config/rusty/.env; never paste tokens into chat or command arguments. Git uses your existing SSH/HTTPS credential helper. /integrations check tests repository access; /integrations prs lists open PRs. Bitbucket Data Center requires a separate API adapter.".into(),
    })
}
/// Explicit reads only. All HTTP credentials stay in memory, never argv or
/// config. No redirect/retry can forward or replay a credentialed request.
fn bitbucket(url: &str, access: Option<String>, basic: Option<(String, String)>) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client.get(url).header("Accept", "application/json");
    if let Some(token) = access {
        request = request.bearer_auth(token);
    } else if let Some((email, token)) = basic {
        request = request.basic_auth(email, Some(token));
    } else {
        bail!("configure BITBUCKET_ACCESS_TOKEN, or BITBUCKET_EMAIL and BITBUCKET_API_TOKEN");
    }
    let response = request.send().map_err(|_| anyhow::anyhow!("Bitbucket connection failed"))?;
    ensure!(
        response.status().is_success(),
        "Bitbucket returned HTTP {}; check credentials and repository access",
        response.status().as_u16()
    );
    let mut bytes = Vec::new();
    response.take(1_048_577).read_to_end(&mut bytes).context("reading Bitbucket response")?;
    ensure!(bytes.len() <= 1_048_576, "Bitbucket response exceeded 1 MiB");
    serde_json::from_slice(&bytes).context("invalid Bitbucket JSON response")
}
pub fn command(backend: &Backend, policy: &Policy, cwd: &Path, path: &Path, args: &[String]) -> Result<String> {
    let action = args.first().map(String::as_str).unwrap_or("status");
    if action == "mcp" || (action == "setup" && args.get(1).is_some_and(|s| s == "mcp")) {
        ensure!(backend.name() == "local", "manage controller MCP configuration before attaching remote tools");
        let sub = if action == "mcp" { args[1..].to_vec() } else { vec!["help".into()] };
        return crate::mcp::manage(cwd, policy, &sub);
    }
    if action == "help" {
        return Ok("Integrations: status | setup github|gitlab|bitbucket|mcp | configure auto|github|gitlab|bitbucket [remote] | check | prs | git status|diff|log | mcp list|add|remove|check. Git/host writes stay on the existing permission-checked tools.".into());
    }

    let mut config = load(path)?;
    if action == "configure" {
        ensure!((2..=3).contains(&args.len()), "usage: /integrations configure auto|github|gitlab|bitbucket [remote]");
        config.provider = if args[1] == "auto" { None } else { Some(provider(&args[1])?.to_string()) };
        if let Some(remote) = args.get(2) {
            config.remote = Some(remote.clone());
        }
        let repo = repository(backend, policy, cwd, &config)?;
        let name = infer(&repo, &config)?;
        rusty::privacy::private_dir(path.parent().context("missing project config directory")?)?;
        rusty::privacy::write_json(path, serde_json::to_value(&config)?)?;
        return Ok(format!(
            "configured {name}: {}/{} via {} (project only; no credentials stored)",
            repo.host,
            repo.path,
            config.remote.as_deref().unwrap_or("origin")
        ));
    }
    if action == "setup" {
        ensure!(args.len() <= 2, "usage: /integrations setup [github|gitlab|bitbucket]");
        let repo = repository(backend, policy, cwd, &config).ok();
        let name = args
            .get(1)
            .cloned()
            .or_else(|| repo.as_ref().and_then(|r| infer(r, &config).ok()))
            .unwrap_or("github".into());
        // A different provider's remote must not become a login destination.
        let compatible = repo.as_ref().filter(|r| infer(r, &config).is_ok_and(|n| n == name));
        return setup(&name, compatible);
    }
    if action == "git" {
        ensure!(args.len() <= 2, "usage: /integrations git [status|diff|log]");
        return run(
            backend,
            policy,
            cwd,
            match args.get(1).map(String::as_str).unwrap_or("status") {
                "status" => "git --no-optional-locks status --short --branch",
                "diff" => "git --no-pager diff --no-ext-diff --no-textconv",
                "log" => "git --no-pager log -10 --oneline",
                _ => bail!(
                    "native Git views are status, diff and log; use normal agent tools for permission-checked writes"
                ),
            },
        );
    }
    ensure!(
        args.len() <= 1 && matches!(action, "status" | "check" | "prs"),
        "usage: /integrations [status|check|prs|setup [provider]|configure provider [remote]|git [view]]"
    );
    let repo = repository(backend, policy, cwd, &config)?;
    let name = infer(&repo, &config)?;
    if action == "status" {
        let state = run(backend, policy, cwd, "git --no-optional-locks status --short --branch")?;
        return Ok(format!("{name}: {}/{}\nremote: {}\n{state}\nAuthentication not checked. /integrations setup shows setup; /integrations check tests access; /integrations prs lists up to 20 open reviews. Git writes, PR creation and merges use the existing permission-checked agent tools.", repo.host, repo.path, config.remote.as_deref().unwrap_or("origin")));
    }
    match name.as_str() {
        "github" | "gitlab" => {
            let cli = if name == "github" { "gh" } else { "glab" };
            let command = if action == "check" {
                format!("{cli} auth status --hostname {}", quote(&repo.host))
            } else if name == "github" {
                format!(
                    "gh pr list --repo {} --limit 20 --json number,title,url,headRefName,baseRefName",
                    quote(&format!("{}/{}", repo.host, repo.path))
                )
            } else {
                format!(
                    "glab mr list --repo {} --per-page 20 --output json",
                    quote(&format!("https://{}/{}", repo.host, repo.path))
                )
            };
            run(backend, policy, cwd, &command)
        }
        "bitbucket" => {
            ensure!(
                repo.host == "bitbucket.org" && repo.path.split('/').count() == 2,
                "only Bitbucket Cloud is supported by this adapter"
            );
            let mut url = format!("https://api.bitbucket.org/2.0/repositories/{}", repo.path);
            if action == "prs" {
                url.push_str("/pullrequests?pagelen=20&state=OPEN");
            }
            // This GET stays on the controller; it neither invokes a shell
            // download nor transfers its credential to a remote tool backend.
            let verdict = policy.decide("integration_read", &json!({"provider":"bitbucket", "operation":action}), cwd);
            ensure!(matches!(verdict, Verdict::Allow), "integration read refused by permissions: {verdict:?}");
            let access = std::env::var("BITBUCKET_ACCESS_TOKEN").ok().filter(|t| !t.trim().is_empty());
            let basic = std::env::var("BITBUCKET_EMAIL").ok().zip(std::env::var("BITBUCKET_API_TOKEN").ok());
            let data = bitbucket(&url, access, basic)?;
            if action == "check" {
                return Ok(format!("Bitbucket repository access verified: {}", repo.path));
            }
            let values = data["values"].as_array().context("Bitbucket response has no PR list")?;
            let list: Vec<_> = values.iter().map(|v| json!({"id":v["id"], "title":v["title"], "url":v["links"]["html"]["href"], "head":v["source"]["branch"]["name"], "base":v["destination"]["branch"]["name"]})).collect();
            Ok(rusty::privacy::redact(&json!({"reviews":list,"more_available":data["next"].is_string()}).to_string()).0)
        }
        _ => unreachable!(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remotes_and_self_hosted_forges_are_explicit_and_cannot_inject_commands() {
        for u in ["git@github.com:org/repo.git", "https://github.com/org/repo.git", "ssh://git@github.com/org/repo"] {
            assert_eq!(remote_url(u).unwrap(), Repo { host: "github.com".into(), path: "org/repo".into() });
        }
        let r = remote_url("git@git.company.test:group/sub/repo.git").unwrap();
        assert!(infer(&r, &Config::default()).is_err());
        assert_eq!(infer(&r, &Config { provider: Some("gitlab".into()), remote: None }).unwrap(), "gitlab");
        for u in [
            "/tmp/repo",
            "https://token:password@github.com/a/b",
            "git@github.com:a/../b",
            "git@github.com:a/$(whoami)",
            "git@github.com:-R/other",
        ] {
            assert!(remote_url(u).is_err(), "{u}");
        }
        assert!(!setup("gitlab", None).unwrap().contains("github.com"));
    }
    #[test]
    fn bitbucket_errors_never_echo_response_bodies_or_follow_redirects() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        for status in ["401 Unauthorized", "302 Found"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.to_lowercase().contains("authorization: bearer test-secret"));
                write!(stream,"HTTP/1.1 {status}\r\nLocation: http://127.0.0.1:1/leak\r\nContent-Length: 11\r\nConnection: close\r\n\r\ntest-secret").unwrap();
            });
            let err = bitbucket(&url, Some("test-secret".into()), None).unwrap_err().to_string();
            assert!(!err.contains("test-secret"));
            server.join().unwrap();
        }
    }
    #[test]
    fn bitbucket_basic_auth_and_bounded_json_are_exercised_over_http() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        for (body, expected) in [
            (br#"{"values":[],"next":"https://untrusted.example"}"#.to_vec(), "ok"),
            (b"not json".to_vec(), "invalid Bitbucket JSON"),
            (vec![b'x'; 1_048_577], "exceeded 1 MiB"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/2.0/repositories/w/r/pullrequests", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.starts_with("GET /2.0/repositories/w/r/pullrequests "));
                assert!(headers.to_lowercase().contains("authorization: basic dtpw"));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len())
                    .unwrap();
                let _ = stream.write_all(&body);
            });
            let result = bitbucket(&url, None, Some(("u".into(), "p".into())));
            if expected == "ok" {
                assert!(result.unwrap()["values"].as_array().unwrap().is_empty());
            } else {
                assert!(result.unwrap_err().to_string().contains(expected));
            }
            server.join().unwrap();
        }
    }
    #[test]
    fn native_http_reads_do_not_grant_write_authority_or_bypass_deny_rules() {
        use crate::permissions::{Mode, Verdict};
        let mut p = Policy::new(Mode::ReadOnly);
        let args = json!({"provider":"bitbucket", "operation":"prs"});
        assert_eq!(p.decide("integration_read", &args, Path::new(".")), Verdict::Allow);
        assert!(matches!(
            p.decide("integration_read", &json!({"provider":"bitbucket", "operation":"delete"}), Path::new(".")),
            Verdict::Deny(_)
        ));
        p.deny.push("integration_read".into());
        assert!(matches!(p.decide("integration_read", &args, Path::new(".")), Verdict::Deny(_)));
    }
}
