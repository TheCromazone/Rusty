//! End-to-end tests that drive the real binary.
//!
//! The default set runs offline: slash commands, settings, permissions,
//! memory, skills and argument handling, all with an isolated config dir.
//! Tests marked `#[ignore]` talk to the real model and need NVIDIA_API_KEY:
//!
//!     cargo test -- --ignored

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "support/sync.rs"]
mod sync;

const BIN: &str = env!("CARGO_BIN_EXE_rusty");

struct Sandbox {
    home: PathBuf,
    project: PathBuf,
    /// Fake `kubectl`, `terraform` and friends go here, first on PATH, so no
    /// test can reach a real cluster or account.
    bin: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        // macOS's per-user temporary directory can leave no room for the
        // memory daemon's Unix socket name. Match the other offline fixtures.
        let parent = if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let root = parent.join(format!("rusty-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let home = root.join("home");
        let project = root.join("project");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        Self { home, project, bin }
    }

    /// Installs a fake command. It appends its arguments to `calls.log` in
    /// the sandbox root, then runs `body` with them.
    fn fake(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = self.bin.join(name);
        std::fs::write(&path, format!("#!/usr/bin/env bash\necho \"{name} $*\" >> \"$RUSTY_TEST_CALLS\"\n{body}\n"))
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        if name == "kubectl" {
            // rusty only asks kubectl about a cluster when one is configured.
            std::fs::create_dir_all(self.home.join(".kube")).unwrap();
            std::fs::write(self.home.join(".kube/config"), "").unwrap();
        }
    }

    fn calls(&self) -> String {
        std::fs::read_to_string(self.bin.parent().unwrap().join("calls.log")).unwrap_or_default()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.project)
            .env("RUSTY_HOME", &self.home)
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .env("RUSTY_TEST_CALLS", self.bin.parent().unwrap().join("calls.log"))
            .env("RUSTY_NO_DOTENV", "1")
            .env("NVIDIA_API_KEY", "offline-test-key")
            // Pinned, so a run asks no endpoint for its model list first.
            .env("RUSTY_CONTEXT_TOKENS", "128000")
            .env("NO_COLOR", "1")
            .env_remove("AWS_PROFILE")
            .env_remove("AWS_DEFAULT_PROFILE")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_REGION")
            .env_remove("AWS_DEFAULT_REGION")
            .env_remove("CLOUDSDK_CORE_PROJECT")
            .env_remove("TF_WORKSPACE")
            .env_remove("KUBECONFIG")
            .env_remove("RUSTY_INFRA")
            .env_remove("RUSTY_MAX_REQUESTS")
            .env_remove("RUSTY_MAX_BUDGET_TOKENS")
            .env_remove("RUSTY_BUDGET_SECS")
            .env_remove("RUSTY_MODEL")
            .env_remove("RUSTY_MODE")
            .env_remove("RUSTY_CAREFUL_MODEL")
            .env_remove("RUSTY_STANDARD_MODEL")
            .env_remove("RUSTY_VIBE_MODEL")
            .env_remove("RUSTY_AGENTS")
            .env_remove("RUSTY_PERMISSIONS")
            .env_remove("RUSTY_PROVIDER")
            .env_remove("CODEX_HOME")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_BASE_URL")
            .env_remove("OPENAI_API_KEY")
            .env_remove("OPENAI_BASE_URL")
            .env_remove("RUSTY_API_KEY")
            .env_remove("RUSTY_BASE_URL")
            .env_remove("NVIDIA_API_BASE");
        // A developer's rotation keys would join the offline key and reach the network.
        for n in 2..=9 {
            c.env_remove(format!("NVIDIA_API_KEY_{n}"));
        }
        c
    }

    /// Runs the REPL with `lines` piped to stdin.
    fn repl(&self, lines: &[&str]) -> String {
        self.repl_with(self.cmd(), lines)
    }

    /// `repl` with memory off.
    fn repl_off(&self, lines: &[&str]) -> String {
        let mut c = self.cmd();
        c.args(["--memory", "off"]);
        self.repl_with(c, lines)
    }

    fn repl_with(&self, mut c: Command, lines: &[&str]) -> String {
        let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        for l in lines {
            writeln!(stdin, "{l}").unwrap();
        }
        drop(stdin);
        let out = wait(child, Duration::from_secs(20));
        assert!(out.status.success(), "repl failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // rusty starts a memory daemon on demand; don't leave it running.
        if self.home.join("memory/advisor.sock").exists() {
            let _ = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd"))
                .arg("--home")
                .arg(&self.home)
                .arg("stop")
                .stdout(Stdio::null())
                .status();
        }
        if let Some(root) = self.home.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn wait(child: std::process::Child, limit: Duration) -> Output {
    sync::wait(child, limit.max(Duration::from_secs(120)))
}

#[test]
fn version_and_help() {
    let s = Sandbox::new("version");
    let out = s.cmd().arg("--version").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("rusty "));
    let out = s.cmd().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--model", "--permissions", "--agents", "--view", "--goal", "--continue", "--stats"] {
        assert!(help.contains(flag), "--help is missing {flag}");
    }
}

#[test]
fn saved_memory_and_history_do_not_contain_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sandbox::new("private-persistence");
    s.repl(&["/remember DB_PASSWORD=cli-secret-canary-12345", "/agents off", "/allow read_file", "/exit"]);
    fn inspect(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                inspect(&p);
            } else if p.is_file() {
                // Bytes, so the memory store's SQLite file is searched too.
                let text = String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned();
                assert!(!text.contains("cli-secret-canary-12345"), "credential persisted in {}", p.display());
                if p.file_name().unwrap() == "history" || p.extension().is_some_and(|s| s == "json") {
                    assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "{}", p.display());
                }
            }
        }
    }
    inspect(&s.home);
}

#[test]
fn missing_key_is_a_clear_error() {
    let s = Sandbox::new("nokey");
    let mut c = s.cmd();
    c.env_remove("NVIDIA_API_KEY").env_remove("RUSTY_API_KEY");
    for i in 2..=9 {
        c.env_remove(format!("NVIDIA_API_KEY_{i}"));
    }
    let out = c.arg("hello").output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no API key found"));
}

#[test]
fn bad_flags_fail_fast() {
    let s = Sandbox::new("flags");
    let out = s.cmd().args(["--view", "sideways", "hi"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown view"));
    let out = s.cmd().args(["--permissions", "maybe", "hi"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown permission mode"));
}

#[test]
fn banner_and_help_list_every_command() {
    let s = Sandbox::new("help");
    let out = s.repl(&["/help"]);
    assert!(out.contains("model") && out.contains("nemotron"), "banner missing model");
    for cmd in [
        "/goal",
        "/loop",
        "/agents",
        "/swarm",
        "/permissions",
        "/memory",
        "/context",
        "/view",
        "/theme",
        "/review",
        "/target",
        "/audit",
        "/triage",
        "/change",
        "/postmortem",
        "/resume",
    ] {
        assert!(out.contains(cmd), "/help is missing {cmd}");
    }
    assert!(out.contains("link closed"), "no goodbye on EOF");
}

#[test]
fn sessions_get_ids_titles_listing_search_and_resume() {
    let s = Sandbox::new("sessions-browser");
    let (out, _, _) = scripted_run(&s, &["make the parser faster"], vec![text_reply("done")]);
    let full_id = out
        .split("rusty -c ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .map(|id| id.trim_matches('`'))
        .unwrap_or_default();
    assert_eq!(full_id.len(), 32, "resume hint should retain the full random ID:\n{out}");
    let id = &full_id[..8];
    let list = |args: &[&str]| {
        let mut command = s.cmd();
        command.env_remove("RUSTY_API_KEY").env_remove("NVIDIA_API_KEY").env_remove("OPENAI_API_KEY");
        for n in 2..=9 {
            command.env_remove(format!("NVIDIA_API_KEY_{n}"));
        }
        let out = command.args(args).stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let all = list(&["--sessions"]);
    assert!(all.contains(full_id) && all.contains("make the parser faster") && all.contains("2 msgs"), "{all}");
    assert!(list(&["--sessions", "parser speed"]).contains(full_id));
    assert!(list(&["--sessions", "kubernetes", "parser"]).contains(full_id), "every unquoted word is searched");
    assert!(!list(&["--sessions", "kubernetes"]).contains(full_id));
    let (out, _, requests) = scripted_run(&s, &["-c", &id[..4], "and the lexer?"], vec![text_reply("sure")]);
    assert!(out.contains("resumed") && out.contains(id) && out.contains("make the parser faster"), "{out}");
    assert!(requests[0]["messages"].to_string().contains("make the parser faster"), "history was not carried over");
    let (out, _, _) = scripted_run(&s, &["-c", "and now?"], vec![text_reply("ok")]);
    assert!(out.contains("resumed") && out.contains("4 messages"), "{out}");
    let missing = s.cmd().args(["-c", "deadbeef", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("no session deadbeef"));
    let repl = s.repl(&["/resume"]);
    assert!(repl.contains(id) && repl.contains("picks one up"), "{repl}");
}

#[test]
fn resume_shows_saved_conversation_without_a_model_request() {
    let s = Sandbox::new("resume-preview");
    scripted_run(&s, &["--memory", "off", "fix the parser"], vec![text_reply("Parser fixed; tests pass.")]);
    let out = s.cmd().args(["--memory", "off", "-c"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success());
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("Earlier in this task") && out.contains("Parser fixed; tests pass."), "{out}");
    let id = sessions_listed(&s, &[])[0].0.clone();
    let out = s.repl_off(&[&format!("/resume {id}")]);
    assert!(out.contains("Earlier in this task") && out.contains("Parser fixed; tests pass."), "{out}");
}

#[test]
fn clear_and_resume_without_trajectory_do_not_use_the_temp_archive() {
    let s = Sandbox::new("archive-disabled");
    scripted_run(&s, &["--memory", "off", "--mode", "standard", "FIRST"], vec![text_reply("first reply")]);
    let id = sessions_listed(&s, &[])[0].0.clone();
    let blocker = s.home.join("not-a-temp-directory");
    std::fs::write(&blocker, b"preserve").unwrap();
    let (url, server) = scripted_endpoint(vec![say("second reply")]);
    let mut cmd = s.cmd();
    cmd.env("TMPDIR", &blocker).env("RUSTY_BASE_URL", url).args(["--memory", "off", "--mode", "standard"]);
    let out = s.repl_with(cmd, &["SECOND", &format!("/resume {id}"), "/clear", "/exit"]);
    assert_eq!(server.join().unwrap().len(), 1);
    assert!(out.contains("resumed") && out.contains("fresh context"), "{out}");
    assert!(!out.contains("error:"), "clear/resume attempted unwanted archive IO: {out}");
    assert_eq!(std::fs::read(&blocker).unwrap(), b"preserve");
}

#[test]
fn explicit_trajectory_preserves_clear_resume_order_and_redaction_in_session_storage() {
    let s = Sandbox::new("archive-export");
    std::fs::write(s.project.join("fixture.txt"), "first tool result").unwrap();
    scripted_run(
        &s,
        &["--memory", "off", "--mode", "standard", "FIRST"],
        vec![tool_reply("read_file", serde_json::json!({"path":"fixture.txt"})), text_reply("first reply")],
    );
    let id = sessions_listed(&s, &[])[0].0.clone();
    let blocker = s.home.join("not-a-temp-directory");
    std::fs::write(&blocker, b"preserve").unwrap();
    let export = s.home.join("trajectory.json");
    let (url, server) = scripted_endpoint(vec![say("second reply"), say("third reply")]);
    let mut cmd = s.cmd();
    cmd.env("TMPDIR", &blocker)
        .env("RUSTY_BASE_URL", url)
        .args(["--memory", "off", "--mode", "standard", "--trajectory"])
        .arg(&export);
    let out = s.repl_with(
        cmd,
        &["SECOND password = ARCHIVE_EXPORT_CANARY", &format!("/resume {id}"), "/clear", "THIRD", "/exit"],
    );
    assert_eq!(server.join().unwrap().len(), 2);
    assert!(!out.contains("error:"), "explicit export used global temporary storage: {out}");
    let raw = std::fs::read_to_string(&export).unwrap();
    assert!(!raw.contains("ARCHIVE_EXPORT_CANARY"));
    let trajectory: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let saved_history = |marker: &str| -> Vec<serde_json::Value> {
        std::fs::read_dir(sessions_dir(&s))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension()?.to_str()? != "json" {
                    return None;
                }
                let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
                let history = value["history"].as_array()?;
                history
                    .iter()
                    .any(|m| m["role"] == "user" && m["content"].as_str().is_some_and(|t| t.starts_with(marker)))
                    .then(|| history.clone())
            })
            .next()
            .expect("saved session history")
    };
    let mut expected = saved_history("SECOND");
    expected.extend(saved_history("FIRST"));
    expected.extend(saved_history("THIRD"));
    assert!(expected.iter().any(|m| m["role"] == "tool"), "fixture did not exercise tool history");
    assert_eq!(trajectory["messages"], serde_json::Value::Array(expected));
    assert_eq!(std::fs::read(&blocker).unwrap(), b"preserve");
}

/// `rusty --sessions [words]` as (id, "age  n msgs  title") pairs.
fn sessions_listed(s: &Sandbox, words: &[&str]) -> Vec<(String, String)> {
    let out = s.cmd().arg("--sessions").args(words).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    let lines: Vec<&str> = out.lines().collect();
    lines
        .windows(2)
        .filter(|w| w[0].starts_with("  ") && !w[0].starts_with("   ") && w[1].starts_with("    "))
        .map(|w| (w[0].trim().trim_end_matches('…').to_string(), w[1].trim().to_string()))
        .collect()
}

/// Runs the REPL against a scripted endpoint that answers `answers` turns.
fn scripted_repl(s: &Sandbox, lines: &[&str], answers: usize) -> String {
    let (url, server) = scripted_endpoint((0..answers).map(|n| say(&format!("answer {n}"))).collect());
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    let out = s.repl_with(cmd, lines);
    assert_eq!(server.join().unwrap().len(), answers, "{out}");
    out
}

#[test]
fn sessions_keep_their_own_title_and_count_across_clear_and_resume() {
    let s = Sandbox::new("sessions-titles");
    let out =
        scripted_repl(&s, &["alpha topic about the zebra bug", "/clear", "beta question about the yak", "/quit"], 2);
    let listed = sessions_listed(&s, &[]);
    let alpha = listed.iter().find(|(_, rest)| rest.ends_with("alpha topic about the zebra bug")).expect("alpha");
    let beta = listed.iter().find(|(_, rest)| rest.ends_with("beta question about the yak")).expect("beta");
    assert!(alpha.1.contains(" 2 msgs") && beta.1.contains(" 2 msgs"), "{listed:?}");
    let yak = sessions_listed(&s, &["yak"]);
    assert_eq!(yak.len(), 1, "{yak:?}");
    assert_eq!(yak[0].0, beta.0);
    assert!(out.contains(&format!("resume with `rusty -c {}`", beta.0)), "the goodbye names the session: {out}");

    let resume = format!("/resume {}", &alpha.0[..12]);
    scripted_repl(&s, &["gamma question", "/clear", &resume, "delta question", "/quit"], 2);
    let listed = sessions_listed(&s, &[]);
    let alpha_now = listed.iter().find(|(id, _)| *id == alpha.0).expect("alpha kept");
    assert!(alpha_now.1.ends_with(" 4 msgs  alpha topic about the zebra bug"), "{listed:?}");
    let gamma = listed.iter().find(|(_, rest)| rest.ends_with("gamma question")).expect("gamma");
    assert!(gamma.1.contains(" 2 msgs"), "{listed:?}");
    assert_eq!(sessions_listed(&s, &["delta"])[0].0, alpha.0);
    assert_eq!(listed.len(), 3, "the session left empty by /clear then /resume is not listed: {listed:?}");
}

#[test]
fn an_empty_repl_leaves_no_session_and_continue_takes_the_last_spoken_one() {
    let s = Sandbox::new("sessions-empty");
    let (out, _, _) = scripted_run(&s, &["--memory", "off", "remember the walrus"], vec![text_reply("ok")]);
    let id = out.split("rusty -c ").nth(1).and_then(|t| t.split_whitespace().next()).unwrap().trim_matches('`');
    let out = s.repl_off(&["/help", "/clear", "/quit"]);
    assert!(out.contains("link closed") && !out.contains("resume with"), "nothing to resume: {out}");
    let listed = sessions_listed(&s, &[]);
    assert_eq!(listed.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(), [id], "{listed:?}");
    let project = std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path();
    let files = std::fs::read_dir(project.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|name| name.ends_with(".json") || name.ends_with(".lock") && name != "index.lock")
        .count();
    assert_eq!(files, 1, "only the spoken session's file remains; released leases leave no lock file");
    let (out, _, requests) = scripted_run(&s, &["--memory", "off", "-c", "what did I ask?"], vec![text_reply("ok")]);
    assert!(out.contains(&format!("resumed {id}")), "{out}");
    assert!(requests[0]["messages"].to_string().contains("remember the walrus"));
}

#[test]
fn an_empty_parent_keeps_its_file_for_the_side_chats_opened_from_it() {
    let s = Sandbox::new("sessions-empty-side");
    let out = s.repl_off(&["/side new notes", "/back", "/quit"]);
    assert!(out.contains("link closed") && !out.contains("resume with"), "{out}");
    let parents = std::fs::read_dir(sessions_dir(&s))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|name| name.ends_with(".json"))
        .count();
    assert_eq!(parents, 1, "the side's parent transcript is kept");
    assert!(sessions_listed(&s, &[]).is_empty(), "but it is not listed");
}

/// The sessions directory of the sandbox's project, after a first run made it.
fn sessions_dir(s: &Sandbox) -> PathBuf {
    std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path().join("sessions")
}

/// Writes a saved session with one ask, as an older rusty or a copy would.
fn write_session(s: &Sandbox, id: &str, ask: &str) {
    let body =
        serde_json::json!({"history": [{"role": "user", "content": ask}, {"role": "assistant", "content": "ok"}]});
    std::fs::write(sessions_dir(s).join(format!("{id}.json")), body.to_string()).unwrap();
}

#[test]
fn a_search_with_no_hit_and_a_shared_id_prefix_say_so() {
    let s = Sandbox::new("sessions-messages");
    scripted_run(&s, &["--memory", "off", "first ask"], vec![text_reply("ok")]);
    std::fs::remove_file(sessions_dir(&s).join("index.sqlite")).unwrap();
    write_session(&s, "aa110000aa110000aa110000aa110000", "first parser bug");
    write_session(&s, "aa220000aa220000aa220000aa220000", "second parser rewrite");

    let out = s.cmd().args(["--sessions", "kubernetes"]).stdin(Stdio::null()).output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    assert!(out.contains("no session matches `kubernetes`") && !out.contains("no sessions here"), "{out}");

    let out = s.cmd().args(["-c", "aa", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("aa110000aa110000") && err.contains("aa220000aa220000"), "{err}");
    assert!(err.contains("give more of the id"), "{err}");

    let out = s.repl_off(&["/resume aa", "/resume nothingmatches", "/resume aa22"]);
    assert!(out.contains("first parser bug") && out.contains("second parser rewrite"), "{out}");
    assert!(out.contains("`aa` could be any of these; give more of the id"), "{out}");
    assert!(out.contains("no session matches `nothingmatches`"), "{out}");
    assert!(out.contains("resumed aa220000"), "{out}");
}

#[test]
fn a_damaged_session_file_is_named_when_resumed() {
    let s = Sandbox::new("sessions-damaged");
    let (out, _, _) = scripted_run(&s, &["--memory", "off", "an ask long enough to cut"], vec![text_reply("ok")]);
    let id = out.split("rusty -c ").nth(1).and_then(|t| t.split_whitespace().next()).unwrap().trim_matches('`');
    let file = sessions_dir(&s).join(format!("{id}.json"));
    let body = std::fs::read(&file).unwrap();
    std::fs::write(&file, &body[..40]).unwrap();
    for args in [vec!["-c", id, "hello"], vec!["-c", "hello"]] {
        let out = s.cmd().args(&args).stdin(Stdio::null()).output().unwrap();
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(&format!("{id}.json")) && err.contains("damaged"), "{args:?}: {err}");
    }
}

#[test]
fn an_old_session_file_copied_in_after_the_index_is_listed_and_resumed() {
    let s = Sandbox::new("sessions-late");
    scripted_run(&s, &["--memory", "off", "first ask"], vec![text_reply("ok")]);
    assert!(sessions_dir(&s).join("index.sqlite").exists());
    write_session(&s, "1700000000", "fix the websocket reconnect");
    let found = sessions_listed(&s, &["websocket"]);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "1700000000");
    let (out, _, requests) =
        scripted_run(&s, &["--memory", "off", "-c", "1700000000", "follow up"], vec![text_reply("ok")]);
    assert!(out.contains("resumed 1700000000"), "{out}");
    assert!(requests[0]["messages"].to_string().contains("fix the websocket reconnect"));
}

#[test]
fn browser_backfills_legacy_transcripts_but_never_lists_side_children() {
    let s = Sandbox::new("sessions-migration");
    let (out, _, _) = scripted_run(&s, &["make the parser faster"], vec![text_reply("done")]);
    let project = std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path();
    let sessions = project.join("sessions");
    let main_session = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "json") && !p.to_string_lossy().ends_with(".trace.jsonl"))
        .unwrap();
    let body = std::fs::read(&main_session).unwrap();
    let legacy = sessions.join("1700000000-abcdef.json");
    std::fs::write(&legacy, body).unwrap();
    let side_dir = sessions.join("sides");
    std::fs::create_dir_all(&side_dir).unwrap();
    std::fs::write(side_dir.join("side0001.json"), r#"{"side":{"title":"private child","parent_id":"parent"},"history":[{"role":"user","content":"side-only-searchword"}]}"#).unwrap();
    // Simulate the first browser access during an upgrade: legacy transcripts
    // must be indexed even though the active run initialized the new catalog.
    std::fs::remove_file(sessions.join("index.sqlite")).unwrap();
    let listed = s.cmd().args(["--sessions"]).stdin(Stdio::null()).output().unwrap();
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.contains("1700000000-abcdef") && listed.contains("make the parser faster"), "{listed}");
    assert!(!listed.contains("side0001") && !listed.contains("private child"), "{listed}");
    let search = s.cmd().args(["--sessions", "side-only-searchword"]).stdin(Stdio::null()).output().unwrap();
    assert!(!String::from_utf8_lossy(&search.stdout).contains("side0001"));
    let resumed = s.cmd().args(["-c", "1700000000-abcdef"]).stdin(Stdio::null()).output().unwrap();
    assert!(resumed.status.success(), "{}", String::from_utf8_lossy(&resumed.stderr));
    assert!(String::from_utf8_lossy(&resumed.stdout).contains("resumed 1700000000-abcdef"));
    assert!(out.contains("rusty -c "));
}

#[test]
fn settings_persist_across_sessions() {
    let s = Sandbox::new("settings");
    s.repl(&["/theme amber", "/view adhd", "/agents swarm", "/swarm size 16", "/swarm spread 0.8", "/tips off"]);
    let out = s.repl(&["/settings"]);
    for want in ["amber", "adhd", "swarm", "16", "0.8"] {
        assert!(out.contains(want), "setting {want} did not persist:\n{out}");
    }
    assert!(out.contains("tips") && out.contains("off"));
}

#[test]
fn swarm_validates_input() {
    let s = Sandbox::new("swarm");
    let out = s.repl(&["/swarm size 500", "/swarm spread 3", "/swarm models a/x, b/y", "/swarm"]);
    assert!(out.contains("size must be 1-16"));
    assert!(out.contains("spread must be between 0 and 1"));
    assert!(out.contains("a/x, b/y"));
}

#[test]
fn permission_classifier_is_inspectable() {
    let s = Sandbox::new("perms");
    let out = s.repl(&[
        "/permissions check git push --force origin main",
        "/permissions check cargo test 2>&1 | tail",
        "/permissions allow git push",
        "/permissions check git push origin main",
        "/permissions check aws s3 rm s3://prod --recursive",
        "/permissions yolo",
        "/permissions check terraform destroy",
        "/permissions read-only",
        "/permissions check touch x",
        "/permissions check kubectl get pods",
    ]);
    let lines: Vec<&str> = out
        .lines()
        .map(|l| l.trim_start().trim_start_matches('›').trim_start())
        .filter(|l| l.starts_with(['✓', '?', '‼', '✗']))
        .collect();
    assert_eq!(lines.len(), 7, "{out}");
    assert!(lines[0].contains("needs you every time") && lines[0].contains("destructive"), "{out}");
    assert!(lines[1].contains("✓ runs") && lines[1].contains("changes the project"), "{out}");
    assert!(lines[2].contains("✓ runs"), "an allow rule lets a push to main through: {out}");
    assert!(lines[3].contains("needs you every time"), "{out}");
    assert!(lines[4].contains("needs you every time") && lines[4].contains("yolo"), "yolo can't skip it: {out}");
    assert!(lines[5].contains("refused"), "{out}");
    assert!(lines[6].contains("✓ runs") && lines[6].contains("read-only"), "{out}");
    // Allow rules are saved per project.
    let out = s.repl(&["/permissions"]);
    assert!(out.contains("allow git push"));
}

#[test]
fn memory_round_trip() {
    let s = Sandbox::new("memory");
    s.repl(&["/remember gotcha: the schema file is generated, never edit it", "/remember preference: tabs not spaces"]);
    let out = s.repl(&["/memory", "/memory search schema"]);
    // The user's own saves start trusted: confidence 0.65, no outcomes yet.
    assert!(out.contains("schema file is generated, never edit it (confidence 0.65, +0 -0, user)"), "{out}");
    // A preference from /remember applies to every project.
    assert!(out.contains("tabs not spaces") && out.contains("user, global)"), "{out}");
    let id = out
        .lines()
        .find(|l| l.contains("schema file"))
        .and_then(|l| l.split(['[', ']']).nth(1))
        .and_then(|tag| tag.split(':').nth(1))
        .unwrap()
        .to_string();
    let out = s.repl(&[&format!("/forget {id}"), "/memory"]);
    assert!(out.contains(&format!("forgot {id}")), "{out}");
    assert!(!out.contains("schema file is generated"), "{out}");
    // A forgotten lesson stays forgotten.
    let out = s.repl(&["/remember gotcha: the schema file is generated, never edit it"]);
    assert!(out.contains("forgotten"), "{out}");
    assert!(s.repl_off(&["/memory"]).contains("memory is off"));
}

#[test]
fn project_skills_override_builtins() {
    let s = Sandbox::new("skills");
    let dir = s.project.join(".rusty").join("skills");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("review.md"), "---\ndescription: our house review\n---\nReview {{args}}").unwrap();
    std::fs::write(dir.join("ship.md"), "---\ndescription: cut a release\n---\nShip {{args}}").unwrap();
    let out = s.repl(&["/skills"]);
    assert!(out.contains("our house review") && out.contains("project"));
    assert!(out.contains("/ship") && out.contains("cut a release"));
    assert!(out.contains("/explore"), "built-ins still listed");
}

#[test]
fn a_partly_typed_command_runs_when_only_one_matches() {
    let s = Sandbox::new("slash-prefix");
    let out = s.repl(&["/cont", "/mode ca", "/co", "/hlep"]);
    // One command starts with /cont, one mode with ca: both run.
    assert!(out.contains("window") && !out.contains("→ /context"), "{out}");
    assert!(out.contains("execution careful") && !out.contains("→ /mode careful"), "{out}");
    // /co could be /compact or /context, and nothing was on screen to choose from.
    assert!(out.contains("unknown command /co. did you mean /compact, /context"), "{out}");
    assert!(out.contains("unknown command /hlep. did you mean /help?"), "{out}");
}

#[test]
fn unknown_command_and_goal_status() {
    let s = Sandbox::new("unknown");
    let out = s.repl(&["/frobnicate", "/goal", "/loop", "/plan", "/context"]);
    assert!(out.contains("unknown command /frobnicate"));
    assert!(out.contains("no goal"));
    assert!(out.contains("usage: /loop"));
    assert!(out.contains("no plan yet"));
    assert!(out.contains("window"));
}

#[test]
fn target_is_detected_shown_and_production_starts_careful() {
    let s = Sandbox::new("target");
    s.fake("kubectl", "printf 'prod-eu|payments'");
    s.fake("aws", "case \"$*\" in *Account*) printf '123456789012\\n';; *region*) printf 'eu-west-1\\n';; esac");
    std::fs::write(s.project.join("main.tf"), "").unwrap();
    std::fs::create_dir_all(s.project.join(".terraform")).unwrap();
    std::fs::write(s.project.join(".terraform/environment"), "staging").unwrap();
    let out = s.repl(&["/mode", "/target", "/settings"]);
    assert!(out.contains("target looks like production: kube prod-eu/payments"), "{out}");
    assert!(
        out.contains("execution auto") && out.contains("last pick careful (the target looks like production)"),
        "production must start careful: {out}"
    );
    assert!(out.contains("terraform staging"), "{out}");
    let target_lines: String =
        out.lines().filter(|line| line.trim().starts_with("target ") || line.contains("›   target ")).collect();
    assert!(!target_lines.contains("aws "), "no aws profile is configured in the sandbox: {out}");
    // Nothing was saved: the next session with a harmless target is standard again.
    s.fake("kubectl", "printf 'kind-dev|default'");
    let out = s.repl(&["/mode", "/target"]);
    assert!(out.contains("execution auto") && out.contains("kube kind-dev/default"), "{out}");
    assert!(!out.contains("production"), "{out}");
    // The flag wins over the heuristic.
    s.fake("kubectl", "printf 'prod|default'");
    let mut child = s.cmd().args(["--mode", "vibe"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "/mode").unwrap();
    let text = String::from_utf8_lossy(&wait(child, Duration::from_secs(20)).stdout).to_string();
    assert!(text.contains("execution vibe") && text.contains("production"), "{text}");
}

#[test]
fn kubectl_is_not_started_without_a_kubeconfig() {
    let s = Sandbox::new("no-kubeconfig");
    s.fake("kubectl", "printf 'prod-eu|payments'");
    std::fs::remove_file(s.home.join(".kube/config")).unwrap();
    let out = s.repl(&["/mode"]);
    assert!(!out.contains("production") && !out.contains("kube "), "{out}");
    assert!(!s.calls().contains("kubectl"), "{}", s.calls());
    // A kubeconfig named by KUBECONFIG counts.
    std::fs::write(s.project.join("kubeconfig"), "").unwrap();
    let mut child = s
        .cmd()
        .env("KUBECONFIG", s.project.join("kubeconfig"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/mode").unwrap();
    let text = String::from_utf8_lossy(&wait(child, Duration::from_secs(20)).stdout).to_string();
    assert!(text.contains("target looks like production: kube prod-eu/payments"), "{text}");
}

#[test]
fn aws_identity_comes_from_one_sts_call() {
    let s = Sandbox::new("target-aws");
    s.fake("aws", "case \"$*\" in *Account*) printf '123456789012\\n';; esac");
    // Startup reads local config only: no aws CLI, no network.
    let out = s
        .cmd()
        .env("AWS_PROFILE", "ops")
        .env("AWS_DEFAULT_REGION", "eu-west-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = out;
    writeln!(child.stdin.take().unwrap(), "/exit").unwrap();
    let text = String::from_utf8_lossy(&wait(child, Duration::from_secs(20)).stdout).to_string();
    assert!(text.contains("aws ops (eu-west-1)"), "{text}");
    assert!(!s.calls().contains("aws"), "startup ran the aws CLI: {}", s.calls());
    // /target asks for the identity, with exactly one STS call.
    let out = s
        .cmd()
        .env("AWS_PROFILE", "ops")
        .env("AWS_DEFAULT_REGION", "eu-west-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = out;
    writeln!(child.stdin.take().unwrap(), "/target").unwrap();
    let text = String::from_utf8_lossy(&wait(child, Duration::from_secs(20)).stdout).to_string();
    assert!(text.contains("aws ops (123456789012, eu-west-1)"), "{text}");
    assert_eq!(s.calls().matches("aws sts get-caller-identity").count(), 1, "{}", s.calls());
    let out = s.repl(&["/target"]);
    assert!(out.contains("none detected"), "{out}");
}

#[test]
fn secrets_in_tool_output_never_reach_the_model_or_the_session_file() {
    let s = Sandbox::new("redact");
    std::fs::write(
        s.project.join(".env"),
        "DB_HOST=db.internal\nDB_PASSWORD=hunter22-real\nAWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n",
    )
    .unwrap();
    let answer = serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]});
    // The model uses the password by its handle, then tries to print it.
    let use_it = "grep -qx \"DB_PASSWORD=$RUSTY_SECRET_1\" .env && echo works; echo \"got $RUSTY_SECRET_1\"";
    let (url, server) = scripted_endpoint(vec![
        tool_reply("bash", serde_json::json!({"command": "cat .env"})),
        tool_reply("bash", serde_json::json!({ "command": use_it })),
        answer,
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "show me the env file"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let seen = requests[2]["messages"].to_string();
    assert!(!seen.contains("hunter22-real") && !seen.contains("AKIAIOSFODNN7EXAMPLE"), "{seen}");
    assert!(seen.contains("DB_HOST=db.internal") && seen.contains("DB_PASSWORD=${RUSTY_SECRET_1}"), "{seen}");
    assert!(seen.contains("redacted 2 secret values"), "{seen}");
    assert!(seen.contains("works") && seen.contains("got ${RUSTY_SECRET_1}"), "{seen}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("2 secrets redacted"));
    let mut sessions = Vec::new();
    find_files(&s.home, "json", &mut sessions);
    assert!(!sessions.is_empty());
    for p in sessions {
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("hunter22-real"), "{} leaks the secret", p.display());
    }
}

/// Runs one headless prompt against a scripted endpoint and returns
/// (stdout, stderr, the requests the endpoint saw).
fn scripted_run(
    s: &Sandbox,
    args: &[&str],
    replies: Vec<serde_json::Value>,
) -> (String, String, Vec<serde_json::Value>) {
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(30)))
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    (String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string(), requests)
}

fn text_reply(text: &str) -> serde_json::Value {
    serde_json::json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}]})
}

fn bash_reply(command: &str) -> serde_json::Value {
    tool_reply("bash", serde_json::json!({"command": command}))
}

/// A private control channel owns this escaped pipe holder's lifetime. The
/// fixture never stores or signals a PID; dropping STOP also releases it.
#[cfg(unix)]
struct CaptureHolder {
    command: String,
    stop: Option<std::sync::mpsc::Sender<()>>,
    control: Option<std::thread::JoinHandle<bool>>,
}
#[cfg(unix)]
impl CaptureHolder {
    fn new(s: &Sandbox) -> Self {
        use std::io::Read;
        use std::os::unix::{fs::PermissionsExt, net::UnixListener};
        std::fs::set_permissions(&s.home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = s.home.join("capture.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let script = s.home.join("capture.py");
        std::fs::write(
            &script,
            r#"import os,socket,sys
r,w=os.pipe()
if os.fork()==0:
    os.close(r)
    os.setsid()
    control=socket.socket(socket.AF_UNIX)
    control.settimeout(25)
    control.connect(sys.argv[1])
    control.sendall(b'READY')
    os.write(w,b'x')
    os.close(w)
    try: control.recv(1)
    finally: os._exit(0)
os.close(w)
os.read(r,1)
os.close(r)
print('launcher completed',flush=True)
os._exit(0)
"#,
        )
        .unwrap();
        let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\"'\"'"));
        let command = format!("python3 {} {}", quote(&script), quote(&socket));
        let (stop, stopped) = std::sync::mpsc::channel();
        let control = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return false,
                }
            };
            // Darwin can inherit the listener's nonblocking flag on accept.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
            let mut ready = [0; 5];
            stream.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"READY");
            let _ = stopped.recv_timeout(Duration::from_secs(20));
            // Probe before STOP: EOF would mean timeout/natural exit, not
            // evidence that capture returned while a live holder owned pipes.
            stream.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
            let mut byte = [0];
            let live = matches!(stream.read(&mut byte), Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
            stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            if live {
                stream.write_all(b"S").unwrap();
                assert_eq!(stream.read(&mut byte).unwrap(), 0, "holder did not close its owned channel");
            }
            live
        });
        Self { command, stop: Some(stop), control: Some(control) }
    }
    fn finish(mut self) {
        let _ = self.stop.take().unwrap().send(());
        assert!(self.control.take().unwrap().join().unwrap(), "holder must remain live until capture returned");
    }
}
#[cfg(unix)]
impl Drop for CaptureHolder {
    fn drop(&mut self) {
        self.stop.take(); // EOF releases the fixture on assertion failure too.
        if let Some(control) = self.control.take() {
            let _ = control.join();
        }
    }
}

#[test]
#[cfg(unix)]
fn completion_contract_cli_keeps_exit_zero_but_rejects_incomplete_fixed_check_capture() {
    let ordinary = Sandbox::new("capture-ordinary");
    let holder = CaptureHolder::new(&ordinary);
    let (stdout, stderr, requests) = scripted_run(
        &ordinary,
        &["--mode", "standard", "--agents", "off", "--memory", "off", "--permissions", "yolo", "inspect"],
        vec![bash_reply(&holder.command), text_reply("observed")],
    );
    holder.finish();
    assert_eq!(requests.len(), 2);
    let tool_results = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(tool_results.contains("exit code: 0"), "{tool_results}");
    assert!(tool_results.contains("capture_complete: false"), "{tool_results}");
    assert!(!tool_results.contains("timed out"), "{tool_results}");
    assert!(!format!("{stdout}{stderr}").contains("timed out"));

    let fixed = Sandbox::new("capture-fixed");
    let holder = CaptureHolder::new(&fixed);
    let (out, trace, _) = verified_run(
        &fixed,
        &holder.command,
        vec![done_reply(), text_reply("capture remains incomplete")],
        "standard",
        "yolo",
    );
    holder.finish();
    assert_eq!(out.status.code(), Some(2));
    let records = trace["verification"].as_array().unwrap();
    assert_eq!(records.len(), 1, "{trace}");
    assert_eq!(records[0]["exit_code"], 0);
    assert_eq!(records[0]["outcome"], "EvidenceUnavailable", "{trace}");
    assert!(records[0]["observation_error"].as_str().unwrap().contains("capture"));
    assert_ne!(trace["goal_outcome"], "verified");
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
}

const FIXED_CHECK: &str = "python3 -c 'from calc import add; assert add(2,3)==5 and add(-2,3)==1'";

fn verified_run(
    s: &Sandbox,
    command: &str,
    replies: Vec<serde_json::Value>,
    mode: &str,
    permissions: &str,
) -> (Output, serde_json::Value, Vec<serde_json::Value>) {
    verified_run_with_memory(s, command, replies, mode, permissions, "off")
}

fn verified_run_with_memory(
    s: &Sandbox,
    command: &str,
    replies: Vec<serde_json::Value>,
    mode: &str,
    permissions: &str,
    memory: &str,
) -> (Output, serde_json::Value, Vec<serde_json::Value>) {
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        // Same-length edits within one second otherwise reuse Python's
        // timestamp bytecode cache and confound this runtime counterexample.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args([
            "--mode",
            mode,
            "--agents",
            "off",
            "--memory",
            memory,
            "--permissions",
            permissions,
            "--goal",
            "repair calc.py",
            "--verify",
            command,
            "--verify-timeout",
            if command == "sleep 30 & wait" { "1" } else { "30" },
            "--stats",
            "--trajectory",
        ])
        .arg(s.home.join("verified.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(60)))
        .unwrap();
    let trace = serde_json::from_str(&std::fs::read_to_string(s.home.join("verified.json")).unwrap()).unwrap();
    let requests = server.join().unwrap_or_else(|_| {
        panic!(
            "script ended early:\n{}\n{}\n{trace}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out, trace, requests)
}

fn seed_calc(s: &Sandbox) {
    std::fs::write(s.project.join("calc.py"), "def add(a,b):\n    return a - b\n").unwrap();
}

fn fix_calc() -> serde_json::Value {
    tool_reply(
        "edit_file",
        serde_json::json!({"path":"calc.py","old_string":"return a - b","new_string":"return a + b"}),
    )
}

fn done_reply() -> serde_json::Value {
    tool_reply("goal_done", serde_json::json!({"evidence":"I verified everything"}))
}

#[test]
fn completion_contract_fixed_goal_accepts_a_correct_patch_with_fresh_executed_evidence() {
    let s = Sandbox::new("fixed-goal-positive");
    seed_calc(&s);
    let (out, trace, requests) = verified_run(&s, FIXED_CHECK, vec![fix_calc(), done_reply()], "standard", "yolo");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 2);
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    assert_eq!(trace["verification"][0]["input_sha256"], trace["verification"][0]["final_sha256"]);
    assert_eq!(trace["verification"][0]["exit_code"], 0);
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"done\""));
    assert!(requests[0]["messages"].to_string().contains("Fixed acceptance command"));
}

/// In careful mode a plan step's expectation is written before its check
/// and kept when the plan is rewritten without it; completion is refused
/// while the step is untested or its result failed, and the record keeps
/// both sides.
#[test]
fn a_careful_goal_closes_only_when_each_stated_expectation_has_a_passing_result() {
    use serde_json::json;
    let s = Sandbox::new("goal-expectations");
    let plan = |expect: Option<&str>| {
        let mut item = json!({"text": "fix add", "status": "done"});
        if let Some(e) = expect {
            item["expect"] = json!(e);
        }
        tool_reply("plan", json!({"items": [item]}))
    };
    let done =
        |results: serde_json::Value| tool_reply("goal_done", json!({"evidence": "add fixed", "results": results}));
    let (url, server) = scripted_endpoint(vec![
        plan(Some("add(2,3) prints 5")),
        plan(None),
        done(json!([])),
        text_reply("No problems found."),
        done(json!([])),
        plan(Some("python3 -c 'print(add(2,3))' prints 5")),
        tool_reply(
            "plan",
            json!({"items": [{"text": "fix add", "status": "done"}, {"text": "add docs", "status": "done", "expect": "README mentions add"}]}),
        ),
        tool_reply("plan", json!({"items": [{"text": "fix add", "status": "done"}]})),
        done(json!([{"step": 1, "status": "failed", "observed": "printed -1"}])),
        done(json!([{"step": 1, "status": "passed", "observed": "printed 5"}])),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        .args([
            "--yolo",
            "--mode",
            "careful",
            "--agents",
            "off",
            "--memory",
            "off",
            "--goal",
            "fix add",
            "--trajectory",
        ])
        .arg(s.home.join("t.json"))
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 10);
    assert!(requests[0]["messages"].to_string().contains("write in its expect field"));
    assert!(requests[0]["tools"].to_string().contains("\"expect\":"));
    let seen = |i: usize| requests[i]["messages"].to_string();
    // The checker sees the plan as the runtime holds it, not only the last
    // plan call, which left the expectation out.
    assert!(seen(3).contains("expect: add(2,3) prints 5"), "{}", seen(3));
    assert!(seen(5).contains("step 1 (fix add) is untested; expected: add(2,3) prints 5"), "{}", seen(5));
    assert!(seen(5).contains("expect: add(2,3) prints 5"), "a plan rewritten without it keeps the expectation");
    assert!(seen(6).contains("the expectation changed for: fix add"), "{}", seen(6));
    assert!(seen(8).contains("the expectation changed for: add docs (dropped)"), "{}", seen(8));
    assert!(seen(9).contains("step 1 (fix add) failed: printed -1"), "{}", seen(9));
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("t.json")).unwrap()).unwrap();
    let claims = &trace["claims"];
    assert_eq!((claims["unverified"].as_u64(), claims["accepted"].as_u64()), (Some(2), Some(1)), "{claims}");
    assert_eq!((claims["revised_expectations"].as_u64(), claims["sent_to_review"].as_u64()), (Some(2), Some(1)));
    let evidence = trace["goal"]["status"]["Done"].as_str().unwrap();
    assert!(
        evidence
            .contains("1. fix add · expected: python3 -c 'print(add(2,3))' prints 5 · observed: printed 5 · passed"),
        "{evidence}"
    );
}

/// The memory daemon beside the rusty under test, run in the sandbox project.
fn memoryd(s: &Sandbox, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd"))
        .current_dir(&s.project)
        .env("RUSTY_HOME", &s.home)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn memory_learns_only_from_a_goals_fixed_check() {
    for (case, check) in [("pass", FIXED_CHECK), ("fail", "false")] {
        let s = Sandbox::new(&format!("memory-credit-{case}"));
        seed_calc(&s);
        std::fs::write(s.project.join("notes.md"), "calc notes\n").unwrap();
        let saved = memoryd(&s, &["remember", "--kind", "gotcha", "calc.py add must return a + b"]);
        let id = saved.rsplit(' ').next().unwrap().to_string();
        let reflection = serde_json::json!({"used": [id], "lessons": [
            {"kind": "gotcha", "text": "calc.py add sign errors break the fixed calc check", "files": ["calc.py"]}
        ]})
        .to_string();
        let mut replies = vec![fix_calc(), done_reply()];
        if case == "fail" {
            // The failed check reopens the goal for one more reply in this turn.
            replies.push(text_reply("I could not make the check pass."));
        }
        replies.push(text_reply(&reflection));
        let (out, trace, requests) = verified_run_with_memory(&s, check, replies, "standard", "yolo", "reflect");
        let list = memoryd(&s, &["list"]);
        memoryd(&s, &["stop"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("\"memory_mode\":\"reflect\""), "{stderr}");
        let first = requests[0]["messages"].to_string();
        assert!(first.contains("calc.py add must return a + b"), "the lesson was not offered: {first}");
        let reflect = requests.last().unwrap()["messages"].to_string();
        assert!(reflect.contains(&id), "{reflect}");
        if case == "pass" {
            assert_eq!(trace["verification"][0]["outcome"], "Passed");
            assert!(reflect.contains("acceptance check passed"), "{reflect}");
            assert!(
                list.contains("calc.py add must return a + b (confidence") && list.contains("+1 -0, user)"),
                "{list}"
            );
            assert!(list.contains("calc.py add sign errors") && list.contains("reflect, 1 files"), "{list}");
        } else {
            assert!(reflect.contains("acceptance check failed"), "{reflect}");
            assert!(list.contains("+0 -1, user)"), "{list}");
            assert!(!list.contains("sign errors"), "a failed goal wrote a lesson: {list}");
        }
    }
}

#[test]
fn completion_contract_fixed_goal_rejects_unchecked_failed_and_stale_completion_claims() {
    for case in ["unchecked", "failed", "stale"] {
        let s = Sandbox::new(&format!("fixed-goal-{case}"));
        seed_calc(&s);
        let mut replies = match case {
            "unchecked" => vec![],
            "failed" => vec![bash_reply("printf 'exit code: 0\\nall checks passed\\n'; exit 1")],
            _ => vec![
                fix_calc(),
                bash_reply(FIXED_CHECK),
                tool_reply(
                    "edit_file",
                    serde_json::json!({"path":"calc.py","old_string":"return a + b","new_string":"return a - b"}),
                ),
            ],
        };
        replies.extend([done_reply(), text_reply("Still unresolved")]);
        let (out, trace, _) = verified_run(&s, FIXED_CHECK, replies, "standard", "yolo");
        assert_eq!(out.status.code(), Some(2), "{case}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(trace["verification"][0]["outcome"], "Failed", "{case}: {trace}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
    }
}

#[test]
fn completion_contract_completion_is_checked_after_all_calls_in_the_same_reply() {
    let s = Sandbox::new("fixed-goal-batch");
    seed_calc(&s);
    let mut batch = done_reply();
    let regress = serde_json::json!({"index":1,"id":"regress","type":"function","function":{"name":"edit_file","arguments":serde_json::json!({"path":"calc.py","old_string":"return a + b","new_string":"return a - b"}).to_string()}});
    batch["choices"][0]["delta"]["tool_calls"].as_array_mut().unwrap().push(regress);
    let (out, trace, _) =
        verified_run(&s, FIXED_CHECK, vec![fix_calc(), batch, text_reply("unresolved")], "standard", "yolo");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(trace["verification"][0]["outcome"], "Failed");
}

#[test]
fn completion_contract_verification_cannot_change_its_inputs_and_then_pass() {
    let s = Sandbox::new("fixed-goal-mutating-check");
    seed_calc(&s);
    let check="python3 -c 'from pathlib import Path; p=Path(\"calc.py\"); p.write_text(p.read_text().replace(\"a - b\",\"a + b\"))'";
    let (out, trace, _) =
        verified_run(&s, check, vec![done_reply(), text_reply("needs a fresh check")], "standard", "yolo");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(trace["verification"][0]["outcome"], "WorkspaceChanged");
    assert_eq!(trace["verification"][0]["exit_code"], 0);
}

#[test]
fn completion_contract_fixed_verification_cannot_bypass_permissions() {
    let s = Sandbox::new("fixed-goal-denied");
    seed_calc(&s);
    let (out, trace, _) = verified_run(
        &s,
        "printf unauthorized > marker",
        vec![done_reply(), text_reply("permission denied")],
        "standard",
        "read-only",
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(!s.project.join("marker").exists());
    assert_eq!(trace["verification"].as_array().unwrap().len(), 0);
    assert!(trace["messages"].to_string().contains("denied by permissions"));
}

#[test]
fn completion_contract_fixed_verification_is_bounded_and_does_not_accept_timeout() {
    let s = Sandbox::new("fixed-goal-timeout");
    seed_calc(&s);
    let (out, trace, _) =
        verified_run(&s, "sleep 30 & wait", vec![done_reply(), text_reply("timed out")], "standard", "yolo");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(trace["verification"][0]["outcome"], "TimedOut");
}

#[test]
fn completion_contract_blocked_goals_skip_fixed_verification_and_review() {
    let s = Sandbox::new("fixed-goal-blocked");
    seed_calc(&s);
    let blocked = tool_reply("goal_done", serde_json::json!({"evidence":"need input","blocked":true}));
    let (out, trace, requests) = verified_run(&s, "printf bad > marker", vec![blocked], "careful", "yolo");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(requests.len(), 1);
    assert!(!s.project.join("marker").exists());
    assert_eq!(trace["verification"].as_array().unwrap().len(), 0);
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"blocked\""));
}

#[test]
fn completion_contract_fixed_verification_follows_the_careful_review_and_its_edits() {
    let s = Sandbox::new("fixed-goal-review");
    seed_calc(&s);
    let (out, trace, requests) = verified_run(
        &s,
        FIXED_CHECK,
        vec![done_reply(), text_reply("fix the subtraction"), fix_calc(), done_reply()],
        "careful",
        "yolo",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 4);
    assert_eq!(trace["verification"].as_array().unwrap().len(), 1);
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    assert_eq!(trace["goal_outcome"], "verified");
}

#[test]
fn completion_contract_fixed_verification_rejects_invalid_cli_settings() {
    let s = Sandbox::new("fixed-goal-flags");
    for args in [
        vec!["--verify", "true"],
        vec!["--goal", "inspect", "--verify", ""],
        vec!["--goal", "inspect", "--verify", "true", "--verify-timeout", "0"],
    ] {
        assert!(!s.cmd().args(args).output().unwrap().status.success());
    }
}

#[test]
fn verified_repair_grader_accepts_oracle_and_rejects_plausible_wrong_patches() {
    let s = Sandbox::new("verified-repair-grader");
    let grader = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/verified-repair/hidden.py");
    for (body, expected) in [("a + b", true), ("a - b", false), ("5", false), ("abs(a) + abs(b)", false)] {
        std::fs::write(s.project.join("calc.py"), format!("def add(a,b):\n    return {body}\n")).unwrap();
        let out = Command::new("python3").arg(&grader).current_dir(&s.project).output().unwrap();
        assert_eq!(out.status.success(), expected, "{body}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(!s.project.join("hidden.py").exists());
    }
}

#[test]
fn infrastructure_commands_are_audited_and_summarised() {
    let s = Sandbox::new("audit");
    s.fake(
        "kubectl",
        "case \"$*\" in get*) printf 'api-1 Running\\n';; apply*) printf 'deployment.apps/api configured\\n';; esac",
    );
    std::fs::write(s.project.join("deploy.yaml"), "kind: Deployment\n").unwrap();
    let (out, _, _) = scripted_run(
        &s,
        &["--yolo", "--mode", "standard", "roll out the deployment"],
        vec![
            bash_reply("kubectl get pods"),
            bash_reply("kubectl apply -f deploy.yaml"),
            bash_reply("ls"),
            text_reply("done"),
        ],
    );
    assert!(out.contains("change record · 1 change"), "{out}");
    assert!(out.contains("✓ kubectl apply -f deploy.yaml") && out.contains("not verified"), "{out}");
    let mut logs = Vec::new();
    find_files(&s.home, "jsonl", &mut logs);
    let audit = logs.iter().find(|p| p.ends_with("audit.jsonl")).expect("no audit log");
    let lines: Vec<serde_json::Value> =
        std::fs::read_to_string(audit).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 2, "ls is not an infrastructure command: {lines:?}");
    assert_eq!(lines[0]["command"], "kubectl get pods");
    assert_eq!(lines[0]["mutating"], false);
    assert_eq!(lines[1]["verb"], "apply");
    assert_eq!(lines[1]["mutating"], true);
    assert_eq!(lines[1]["outcome"], "ran");
    assert_eq!(lines[1]["exit"], 0);
    assert_eq!(lines[1]["mode"], "standard");
    let mut records = Vec::new();
    find_files(&s.home, "txt", &mut records);
    assert!(records.iter().any(|p| p.to_string_lossy().ends_with(".changes.txt")), "no change record saved");
    // Refusals are logged too, and the log survives across sessions.
    let (out, _, _) = scripted_run(
        &s,
        &["--permissions", "read-only", "delete it"],
        vec![bash_reply("kubectl delete deploy api"), text_reply("could not")],
    );
    assert!(out.contains("denied by permissions"), "{out}");
    // A destructive command needs a person; with nobody watching it is declined and logged.
    let (out, _, _) =
        scripted_run(&s, &["drop it"], vec![bash_reply("terraform destroy -auto-approve"), text_reply("no")]);
    assert!(out.contains("refused"), "{out}");
    assert!(!s.calls().contains("terraform destroy"), "{}", s.calls());
    let out = s.repl(&["/audit", "/changes"]);
    assert!(out.contains("terraform destroy -auto-approve") && out.contains("declined"), "{out}");
    assert!(out.contains("kubectl get pods") && out.contains("read"), "{out}");
    assert!(out.contains("kubectl apply -f deploy.yaml") && out.contains("change"), "{out}");
    assert!(out.contains("kubectl delete deploy api") && out.contains("denied"), "{out}");
    assert!(out.contains("nothing changed in infrastructure this session"), "{out}");
}

#[test]
fn a_change_is_snapshotted_before_it_runs() {
    use std::os::unix::fs::PermissionsExt;
    let s = Sandbox::new("snapshot");
    s.fake(
        "kubectl",
        "case \"$*\" in \
         \"get -f deploy.yaml -o yaml --ignore-not-found -n payments\") printf 'apiVersion: apps/v1\\nkind: Deployment\\nmetadata:\\n  name: api\\n  resourceVersion: \"7\"\\nspec:\\n  replicas: 1\\nstatus:\\n  readyReplicas: 1\\n';; \
         \"get -f new.yaml\"*) exit 0;; \
         apply*) printf 'deployment.apps/api configured\\n';; esac",
    );
    s.fake("helm", "case \"$*\" in \"get values\"*) printf 'image:\\n  tag: v1\\n';; \"get manifest\"*) printf 'kind: Deployment\\n';; upgrade*) printf 'Release api has been upgraded\\n';; esac");
    std::fs::write(s.project.join("deploy.yaml"), "kind: Deployment\n").unwrap();
    let (out, _, requests) = scripted_run(
        &s,
        &["--yolo", "--mode", "standard", "deploy"],
        vec![
            bash_reply("kubectl apply -f deploy.yaml -n payments"),
            bash_reply("kubectl apply -f new.yaml"),
            bash_reply("helm upgrade api ./chart -n payments"),
            text_reply("done"),
        ],
    );
    let log = s.calls();
    // The first call is the target probe at startup.
    let calls: Vec<&str> = log.lines().skip(1).collect();
    assert_eq!(calls[0], "kubectl get -f deploy.yaml -o yaml --ignore-not-found -n payments", "{calls:?}");
    assert_eq!(calls[1], "kubectl apply -f deploy.yaml -n payments");
    assert_eq!(calls[4], "helm get values api -o yaml -n payments");
    assert_eq!(calls[5], "helm get manifest api -n payments");
    assert_eq!(calls[6], "helm upgrade api ./chart -n payments");
    assert!(out.contains("⎘ snapshot") && out.contains("nothing to snapshot: the objects don't exist yet"), "{out}");
    let seen = requests[1]["messages"].to_string();
    assert!(
        seen.contains("pre-change snapshot of the previous state") && seen.contains("Rollback: kubectl apply -f "),
        "{seen}"
    );
    let mut snaps = Vec::new();
    find_files(&s.home, "yaml", &mut snaps);
    let before = snaps.iter().find(|p| p.ends_with("before.yaml")).expect("no before.yaml");
    let dir = before.parent().unwrap();
    assert!(dir.file_name().unwrap().to_string_lossy().ends_with("-kubectl-apply"));
    assert!(std::fs::read_to_string(before).unwrap().contains("resourceVersion"));
    let rollback = std::fs::read_to_string(dir.join("rollback.yaml")).unwrap();
    assert!(
        !rollback.contains("resourceVersion")
            && !rollback.contains("readyReplicas")
            && rollback.contains("replicas: 1"),
        "{rollback}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("command.txt")).unwrap().trim(),
        "kubectl apply -f deploy.yaml -n payments"
    );
    assert_eq!(std::fs::metadata(before).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert!(snaps.iter().any(|p| p.ends_with("values.yaml")) && snaps.iter().any(|p| p.ends_with("manifest.yaml")));
    assert!(out.contains("change record · 3 changes"), "{out}");
    assert!(out.contains("snapshot ") && out.contains("nothing to snapshot"), "{out}");
}

#[test]
fn careful_mode_refuses_an_apply_without_a_fresh_diff() {
    let s = Sandbox::new("gate");
    s.fake("kubectl", "case \"$*\" in diff*) printf 'diff'; exit 1;; apply*) printf 'configured\\n';; esac");
    s.fake("terraform", "case \"$*\" in plan*) touch rusty.tfplan;; esac; printf 'ok\\n'");
    std::fs::write(s.project.join("deploy.yaml"), "replicas: 1\n").unwrap();
    let (out, stderr, requests) = scripted_run(
        &s,
        &["--yolo", "--mode", "careful", "--stats", "deploy"],
        vec![
            bash_reply("kubectl apply -f deploy.yaml"),
            bash_reply("kubectl diff -f deploy.yaml"),
            bash_reply("kubectl apply -f deploy.yaml"),
            tool_reply("edit_file", serde_json::json!({"path": "deploy.yaml", "old_string": "1", "new_string": "2"})),
            bash_reply("kubectl apply -f deploy.yaml"),
            bash_reply("terraform apply"),
            bash_reply("terraform plan -out=rusty.tfplan"),
            bash_reply("terraform apply rusty.tfplan"),
            text_reply("done"),
            text_reply("done"),
            text_reply("done"),
        ],
    );
    let log = s.calls();
    // Leave out the target probe and careful mode's verification commands.
    let calls: Vec<&str> = log
        .lines()
        .filter(|l| !l.contains("config view") && !l.contains("-o name") && !l.contains("-detailed-exitcode"))
        .collect();
    assert_eq!(
        calls,
        [
            "kubectl diff -f deploy.yaml",
            "kubectl get -f deploy.yaml -o yaml --ignore-not-found",
            "kubectl apply -f deploy.yaml",
            "terraform plan -out=rusty.tfplan",
            "terraform state pull",
            "terraform apply rusty.tfplan",
        ],
        "blocked commands must never reach the tool"
    );
    let result = |i: usize| requests[i]["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    assert!(
        result(1).contains("blocked by careful mode: no diff or plan") && result(1).contains("--dry-run=server"),
        "{}",
        result(1)
    );
    assert!(result(3).contains("exit code: 0"), "{}", result(3));
    assert!(result(5).contains("stale"), "{}", result(5));
    assert!(result(6).contains("plan -out=rusty.tfplan"), "{}", result(6));
    assert!(out.contains("blocked by careful mode"), "{out}");
    assert!(out.contains("change record · 5 changes"), "{out}");
    assert!(out.matches("blocked").count() >= 3, "{out}");
    let stats: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    let safety = &stats["safety"];
    assert!(
        safety["risky"]["careful_gate"].as_u64().unwrap() + safety["destructive"]["careful_gate"].as_u64().unwrap() > 0,
        "careful-gate refusals must remain distinct from denials and execution: {safety}"
    );
    // Standard mode only advises; it does not refuse.
    let s2 = Sandbox::new("gate-standard");
    s2.fake("kubectl", "printf 'configured\\n'");
    std::fs::write(s2.project.join("deploy.yaml"), "replicas: 1\n").unwrap();
    scripted_run(
        &s2,
        &["--yolo", "--mode", "standard", "deploy"],
        vec![bash_reply("kubectl apply -f deploy.yaml"), text_reply("done")],
    );
    assert!(s2.calls().contains("kubectl apply -f deploy.yaml"));
}

#[test]
fn careful_mode_verifies_rollouts_after_a_change() {
    let s = Sandbox::new("verify");
    s.fake(
        "kubectl",
        "case \"$*\" in \
         diff*) exit 1;; \
         \"get -f deploy.yaml -o name\"*) printf 'deployment.apps/api\\nservice/api\\n';; \
         \"rollout status deployment.apps/api\"*) printf 'deployment \"api\" successfully rolled out\\n';; \
         \"rollout status deploy/web\"*) printf 'error: timed out waiting for the condition\\n' >&2; exit 1;; \
         apply*|scale*) printf 'ok\\n';; esac",
    );
    s.fake("terraform", "case \"$*\" in plan*-detailed-exitcode*) printf 'No changes.\\n';; *) printf 'ok\\n';; esac");
    std::fs::write(s.project.join("deploy.yaml"), "kind: Deployment\n").unwrap();
    let (out, _, requests) = scripted_run(
        &s,
        &["--yolo", "--mode", "careful", "deploy"],
        vec![
            bash_reply("kubectl diff -f deploy.yaml"),
            bash_reply("kubectl apply -f deploy.yaml"),
            bash_reply("kubectl scale deploy/web --replicas=3"),
            bash_reply("terraform plan -out=p.tfplan"),
            bash_reply("terraform apply p.tfplan"),
            text_reply("done"),
            text_reply("done"),
            text_reply("done"),
        ],
    );
    let log = s.calls();
    assert!(log.contains("kubectl rollout status deployment.apps/api --timeout=180s"), "{log}");
    assert!(log.contains("kubectl rollout status deploy/web --timeout=180s"), "{log}");
    assert!(log.contains("terraform plan -detailed-exitcode -input=false -no-color"), "{log}");
    let result = |i: usize| requests[i]["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    assert!(
        result(2).contains("[verified: deployment.apps/api: deployment \\\"api\\\" successfully rolled out]"),
        "{}",
        result(2)
    );
    assert!(
        result(3).contains("verification FAILED: deploy/web: error: timed out") && result(3).contains("NOT healthy"),
        "{}",
        result(3)
    );
    assert!(result(5).contains("[verified: post-apply plan is empty"), "{}", result(5));
    assert!(out.contains("✓ verified") && out.contains("✗ not verified"), "{out}");
    assert!(out.contains("kubectl apply -f deploy.yaml") && out.contains("· verified"), "{out}");
    assert!(out.contains("kubectl scale deploy/web") && out.contains("NOT verified"), "{out}");
    // Standard mode does not wait.
    let s2 = Sandbox::new("verify-standard");
    s2.fake("kubectl", "printf 'ok\\n'");
    scripted_run(
        &s2,
        &["--yolo", "--mode", "standard", "x"],
        vec![bash_reply("kubectl scale deploy/web --replicas=3"), text_reply("done")],
    );
    assert!(!s2.calls().contains("rollout status"), "{}", s2.calls());
}

fn find_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.is_dir() {
            find_files(&p, ext, out);
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
}

// ------------------------------------------------------------------ live

fn live(name: &str) -> Option<Sandbox> {
    // Live tests use the developer's real key from the environment or .env.
    let _ = dotenvy::from_path(Path::new(env!("CARGO_MANIFEST_DIR")).join(".env"));
    std::env::var("NVIDIA_API_KEY").ok()?;
    Some(Sandbox::new(name))
}

fn live_cmd(s: &Sandbox) -> Command {
    let mut c = Command::new(BIN);
    c.current_dir(&s.project).env("RUSTY_HOME", &s.home).env("NO_COLOR", "1").env("RUSTY_INFRA", "off");
    c
}

#[test]
#[ignore = "requires a live model and provider credentials"]
fn live_fixes_a_bug_and_reports_stats() {
    let Some(s) = live("live-fix") else { return };
    std::fs::write(s.project.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
    std::fs::write(s.project.join("test_calc.py"), "from calc import add\nassert add(2, 3) == 5\nprint('ok')\n")
        .unwrap();
    let out = live_cmd(&s)
        .args(["--stats", "fix calc.py so python3 test_calc.py passes, and run it to check"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let run = Command::new("python3").arg("test_calc.py").current_dir(&s.project).output().unwrap();
    assert!(run.status.success(), "the bug is still there");
    let stats = String::from_utf8_lossy(&out.stderr);
    let line = stats.lines().rev().find(|l| l.starts_with('{')).expect("no --stats line");
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    assert!(v["requests"].as_u64().unwrap() >= 2);
}

#[test]
#[ignore = "requires a live model and provider credentials"]
fn live_goal_mode_closes_with_evidence() {
    let Some(s) = live("live-goal") else { return };
    let out = live_cmd(&s)
        .args([
            "--stats",
            "--goal",
            "create hello.sh that prints hello, make it executable, and run it to prove it works",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("goal done"), "{text}");
    assert!(s.project.join("hello.sh").exists());
}

#[test]
#[ignore = "requires a live model and provider credentials"]
fn live_ctrl_c_stops_quickly() {
    let Some(s) = live("live-sigint") else { return };
    let child = live_cmd(&s)
        .arg("write the numbers one to five thousand in words, one per line")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let sent = Instant::now();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = wait(child, Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(130));
    assert!(sent.elapsed() < Duration::from_secs(3), "took {:?} to stop", sent.elapsed());
}

#[test]
#[ignore = "requires a live model and provider credentials"]
fn live_swarm_runs_workers_in_parallel() {
    let Some(s) = live("live-swarm") else { return };
    for (f, body) in [("a.py", "def alpha():\n    return 1\n"), ("b.py", "def beta():\n    return 2\n")] {
        std::fs::write(s.project.join(f), body).unwrap();
    }
    let out = live_cmd(&s)
        .args([
            "--agents",
            "swarm",
            "use the swarm tool with two workers, one per file (a.py, b.py), to report the function each defines. Then list both names.",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("alpha") && text.contains("beta"), "{text}");
    assert!(text.contains("swarm"), "swarm tool was not used:\n{text}");
}

#[test]
fn execution_modes_persist_without_changing_permissions() {
    let s = Sandbox::new("execution-modes");
    let out = s.repl(&["/mode vibe", "/settings", "/permissions check git push origin main"]);
    assert!(out.contains("execution vibe"), "{out}");
    assert!(out.contains("workers auto (≤3)"), "{out}");
    assert!(out.contains("swarm size      8"), "a mode must not rewrite the saved swarm size: {out}");
    assert!(out.contains("? asks first"), "vibe must not authorize git push: {out}");
    let settings: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("settings.json")).unwrap()).unwrap();
    assert_eq!(settings["execution_mode"], "vibe");
    let out = s.repl(&["/mode", "/agents off", "/mode careful", "/mode vibe"]);
    assert!(out.contains("checker one read-only check"), "{out}");
    let out = s.repl(&["/mode"]);
    assert!(out.contains("workers off"), "explicit delegation choice must persist: {out}");
    let out = s.repl(&["/agents default", "/mode"]);
    assert!(out.contains("workers auto"), "/agents default hands delegation back to the mode: {out}");
}

#[test]
fn execution_mode_cli_and_model_precedence() {
    let s = Sandbox::new("mode-models");
    s.repl(&["/mode careful"]);
    let out = s
        .cmd()
        .args(["--mode", "vibe", "--model", "explicit-model"])
        .env("RUSTY_VIBE_MODEL", "fast-model")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = out;
    writeln!(child.stdin.take().unwrap(), "/mode\n/exit").unwrap();
    let out = wait(child, Duration::from_secs(20));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("execution vibe") && text.contains("model explicit-model"), "{text}");
    // CLI choice must not overwrite the saved mode.
    assert!(s.repl(&["/mode"]).contains("execution careful"));
    let out = s.cmd().args(["--mode", "nonsense", "hi"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown execution mode"));
}

/// A scripted SSE endpoint drives the real loop without paying for inference.
fn scripted_endpoint(replies: Vec<serde_json::Value>) -> (String, std::thread::JoinHandle<Vec<serde_json::Value>>) {
    let bodies = replies.into_iter().map(|r| format!("data: {r}\n\ndata: [DONE]\n\n")).collect();
    let (url, handle) = scripted_sse(bodies);
    (url, std::thread::spawn(move || handle.join().unwrap().into_iter().map(|(_, body)| body).collect()))
}

/// Serves each SSE body in turn; returns every request's headers (lower-cased
/// names) and JSON body.
#[allow(clippy::type_complexity)]
fn scripted_sse(bodies: Vec<String>) -> (String, std::thread::JoinHandle<Vec<(String, serde_json::Value)>>) {
    scripted_http(bodies.into_iter().map(|b| (200, b)).collect())
}

/// Like `scripted_sse`, with a status code per response. A request without
/// a body (a GET) is recorded as null.
#[allow(clippy::type_complexity)]
fn scripted_http(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<(String, serde_json::Value)>>) {
    use std::io::{BufRead, BufReader, Read};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, mut sse) in responses {
            let start = Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(start.elapsed() < Duration::from_secs(120), "expected another request");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut size = 0;
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0, "EOF in scripted request headers");
                if line == "\r\n" {
                    break;
                }
                headers.push_str(&line.to_ascii_lowercase());
                if let Some((key, value)) = line.split_once(':') {
                    if key.eq_ignore_ascii_case("content-length") {
                        size = value.trim().parse().unwrap();
                    }
                }
            }
            let mut body = vec![0; size];
            reader.read_exact(&mut body).unwrap();
            requests.push((
                headers,
                if body.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&body).unwrap() },
            ));
            // Activity IDs come from the real executor, never from the scripted
            // model. Resolve a script placeholder from its observed tool reply.
            if sse.contains("__owned_shell__") {
                let body = &requests.last().unwrap().1;
                let id = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find_map(|m| {
                        let r: serde_json::Value = serde_json::from_str(m["content"].as_str()?).ok()?;
                        r["id"].as_str().filter(|s| s.starts_with("shell-")).map(str::to_string)
                    })
                    .expect("script must observe a real activity handle first");
                sse = sse.replace("__owned_shell__", &id);
            }
            // Rate limits ask for no wait, so retry tests run in milliseconds.
            let retry = if status == 429 { "Retry-After: 0\r\n" } else { "" };
            write!(stream, "HTTP/1.1 {status} OK\r\nContent-Type: text/event-stream\r\n{retry}Content-Length: {}\r\nConnection: close\r\n\r\n{}", sse.len(), sse).unwrap();
        }
        requests
    });
    (url, handle)
}

/// `reply` with the provider reporting `prompt` input tokens.
fn reported(mut reply: serde_json::Value, prompt: u64) -> serde_json::Value {
    reply["usage"] = serde_json::json!({"prompt_tokens": prompt, "completion_tokens": 5});
    reply
}

/// `reply` without any usage, as from an endpoint that reports none.
fn unreported(mut reply: serde_json::Value) -> serde_json::Value {
    reply.as_object_mut().unwrap().remove("usage");
    reply
}

fn tool_reply(name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call", "type": "function",
        "function": {"name": name, "arguments": args.to_string()}}]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}})
}

/// Several calls in one reply: reads may run side by side, but every result
/// comes back under its own call, in order, and a read after a write sees
/// the write. A read that needs approval keeps its place and its refusal.
#[test]
fn reads_in_one_reply_come_back_in_order_and_after_earlier_writes() {
    let s = Sandbox::new("read-batch");
    for (name, text) in [("a.txt", "alpha\n"), ("b.txt", "bravo\n"), ("d.txt", "delta\n")] {
        std::fs::write(s.project.join(name), text).unwrap();
    }
    let calls = [
        ("read_file", serde_json::json!({"path": "a.txt"})),
        ("read_file", serde_json::json!({"path": "b.txt"})),
        ("write_file", serde_json::json!({"path": "c.txt", "content": "charlie\n"})),
        ("read_file", serde_json::json!({"path": "c.txt"})),
        ("glob", serde_json::json!({"pattern": "*.txt"})),
        ("read_file", serde_json::json!({"path": "~/.aws/credentials"})),
        ("read_file", serde_json::json!({"path": "d.txt"})),
        ("list_files", serde_json::json!({"path": "."})),
    ];
    let calls: Vec<serde_json::Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            serde_json::json!({"index": i, "id": format!("call{i}"), "type": "function",
                "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    let batch = serde_json::json!({"choices": [{"delta": {"tool_calls": calls}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
    let (_, _, requests) = scripted_run(
        &s,
        &["--memory", "off", "--mode", "standard", "--permissions", "auto", "read them"],
        vec![batch, text_reply("ok")],
    );
    assert_eq!(requests.len(), 2);
    let results: Vec<(String, String)> = requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| (m["tool_call_id"].as_str().unwrap().to_string(), m["content"].as_str().unwrap().to_string()))
        .collect();
    let ids: Vec<&str> = results.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, (0..8).map(|i| format!("call{i}")).collect::<Vec<_>>());
    let text = |i: usize| results[i].1.as_str();
    assert!(text(0).contains("alpha") && text(1).contains("bravo"), "{results:?}");
    assert!(text(2).starts_with("wrote 1 lines to c.txt"), "{}", text(2));
    assert!(text(3).contains("charlie"), "the read after the write sees it: {}", text(3));
    assert!(text(4).contains("c.txt") && text(7).contains("c.txt"), "{results:?}");
    assert!(text(5).starts_with("error:") && text(5).contains("approval"), "{}", text(5));
    assert!(text(6).contains("delta"), "{}", text(6));
}

#[test]
fn careful_goal_gets_one_checker_review_then_closes() {
    let s = Sandbox::new("careful-goal");
    let done = tool_reply("goal_done", serde_json::json!({"evidence": "checked"}));
    let (stdout, stderr, requests) = scripted_run(
        &s,
        &["--mode", "careful", "--goal", "check the workspace", "--stats"],
        vec![
            done.clone(),
            // The checker, a separate read-only worker.
            text_reply("finding: the empty-input case is not handled (main.py:3)"),
            // The main agent fixes it, verifies, and claims done again: no second review.
            tool_reply("bash", serde_json::json!({"command": "printf fixed-and-tested"})),
            done,
        ],
    );
    assert_eq!(requests.len(), 4);
    let checker = requests[1]["messages"].to_string();
    assert!(checker.contains("read-only reviewer"), "{checker}");
    assert!(!requests[1]["tools"].to_string().contains("goal_done"), "the checker can't close the goal");
    let fix = requests[2]["messages"].to_string();
    assert!(fix.contains("checker's review") && fix.contains("empty-input"), "{fix}");
    assert!(stdout.contains("careful check") && stdout.contains("fixed-and-tested"), "{stdout}");
    assert!(stderr.contains("\"goal\":\"done\""), "{stderr}");
    let stats: serde_json::Value = serde_json::from_str(stderr.lines().find(|l| l.starts_with('{')).unwrap()).unwrap();
    assert_eq!(stats["model_budget"]["by_role"], serde_json::json!({"lead":3,"review":1}));
    assert_eq!(stats["model_budget"]["requests"], 4);
}

#[test]
fn standard_and_vibe_finish_without_extra_reviews() {
    for mode in ["standard", "vibe"] {
        let s = Sandbox::new(mode);
        let (url, server) =
            scripted_endpoint(vec![tool_reply("goal_done", serde_json::json!({"evidence": "checked"}))]);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--mode", mode, "--goal", "inspect", "--stats"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map(|child| wait(child, Duration::from_secs(20)))
            .unwrap();
        assert!(out.status.success());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["max_tokens"], if mode == "vibe" { 8192 } else { 16384 });
        let tools = requests[0]["tools"].to_string();
        assert_eq!(tools.contains("Run read-only workers in parallel"), mode == "vibe");
    }
}

#[test]
fn careful_blocked_goal_does_not_spend_review_calls() {
    let s = Sandbox::new("careful-blocked");
    let (url, server) = scripted_endpoint(vec![tool_reply(
        "goal_done",
        serde_json::json!({"evidence": "need a target", "blocked": true}),
    )]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "careful", "--goal", "inspect", "--stats"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(server.join().unwrap().len(), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"blocked\""));
}

#[test]
fn careful_does_not_review_a_plain_answer() {
    let s = Sandbox::new("careful-answer");
    let reply = serde_json::json!({"choices": [{"delta": {"content": "answer"}, "finish_reason": "stop"}]});
    let (url, server) = scripted_endpoint(vec![reply]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "careful", "explain the code"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert!(out.status.success());
    assert_eq!(server.join().unwrap().len(), 1, "a read-only answer needs no second look");
}

#[test]
fn careful_checks_an_answer_after_a_change() {
    let s = Sandbox::new("careful-change");
    let (_, _, requests) = scripted_run(
        &s,
        &["--mode", "careful", "write a note"],
        vec![
            tool_reply("write_file", serde_json::json!({"path": "note.txt", "content": "hi"})),
            text_reply("done"),
            text_reply("no findings; could not run anything for a text file"),
            text_reply("done, and the checker found nothing"),
        ],
    );
    assert_eq!(requests.len(), 4);
    let note = requests[3]["messages"].to_string();
    assert!(note.contains("checker's review"), "{note}");
    // The user already saw the answer once; the follow-up must not restate it.
    assert!(note.contains("Do not repeat it") && note.contains("nothing to fix"), "{note}");
    assert!(!note.contains("give your final answer"), "{note}");
}

#[test]
fn careful_checks_once_per_goal_not_once_per_goal_turn() {
    let s = Sandbox::new("careful-goal-turns");
    let done = tool_reply("goal_done", serde_json::json!({"evidence": "checked"}));
    // Turn 1: claims done, the checker reviews, the agent answers in text and the turn ends.
    // Turn 2: claims done again and the goal closes without another review.
    let (_, stderr, requests) = scripted_run(
        &s,
        &["--mode", "careful", "--goal", "inspect", "--stats"],
        vec![done.clone(), text_reply("looks fine"), text_reply("still checking"), done],
    );
    assert_eq!(requests.len(), 4);
    assert!(stderr.contains("\"goal\":\"done\""), "{stderr}");
}

#[test]
fn truncated_reply_after_the_check_leaves_the_goal_open_completion_contract() {
    let s = Sandbox::new("careful-truncated");
    let done = tool_reply("goal_done", serde_json::json!({"evidence": "checked"}));
    let mut truncated = text_reply("I'll fix the empty case by");
    truncated["choices"][0]["finish_reason"] = serde_json::json!("length");
    let (url, server) = scripted_endpoint(vec![done, text_reply("finding: empty case"), truncated]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        .args(["--mode", "careful", "--goal", "inspect", "--stats"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(server.join().unwrap().len(), 3);
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
}

#[test]
fn a_link_out_of_the_project_does_not_carry_a_write_with_it() {
    use std::os::unix::fs::symlink;
    let s = Sandbox::new("link-escape");
    // Outside the project, in the (fake) home directory, where writes ask.
    let outside = s.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("notes.md"), "keep\n").unwrap();
    symlink(&outside, s.project.join("escape")).unwrap();
    symlink(outside.join("notes.md"), s.project.join("notes.md")).unwrap();
    let (url, server) = scripted_endpoint(vec![
        tool_reply("write_file", serde_json::json!({"path": "escape/new.txt", "content": "planted\n"})),
        tool_reply("edit_file", serde_json::json!({"path": "notes.md", "old_string": "keep", "new_string": "lost"})),
        tool_reply("write_file", serde_json::json!({"path": "kept.txt", "content": "fine\n"})),
        text_reply("ok"),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--agents", "off", "--memory", "off", "tidy up"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(!outside.join("new.txt").exists(), "a write followed a link out of the project");
    assert_eq!(std::fs::read_to_string(outside.join("notes.md")).unwrap(), "keep\n");
    assert_eq!(std::fs::read_to_string(s.project.join("kept.txt")).unwrap(), "fine\n");
    for i in [1, 2] {
        let told = requests[i]["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
        assert!(told.contains("needs approval"), "{told}");
    }
    let out = s.repl(&["/permissions check echo planted > escape/new.txt", "/permissions check echo ok > kept.txt"]);
    assert!(out.contains("writes outside the project through a link: escape/new.txt"), "{out}");
    assert!(out.contains("runs"), "{out}");
}

#[test]
fn yolo_still_refuses_a_destructive_command_when_nobody_is_watching() {
    let s = Sandbox::new("yolo-destructive");
    // Something precious in a (fake) home directory outside the project.
    let user_home = s.home.parent().unwrap().join("user-home");
    let precious = user_home.join("precious");
    std::fs::create_dir_all(precious.join("data")).unwrap();
    std::fs::write(precious.join("data/keep.txt"), "irreplaceable").unwrap();
    let answer = serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
    let (url, server) = scripted_endpoint(vec![
        tool_reply("bash", serde_json::json!({"command": "rm -rf ~/precious"})),
        tool_reply("bash", serde_json::json!({"command": "rm -rf build && mkdir build && echo hi > build/x"})),
        answer.clone(),
        // The destructive request moved the turn up to careful, so a checker reviews it.
        serde_json::json!({"choices": [{"delta": {"content": "no findings"}, "finish_reason": "stop"}]}),
        answer,
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("HOME", &user_home)
        .args(["--yolo", "clean up"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(20)))
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert!(precious.join("data/keep.txt").exists(), "a destructive command ran unattended");
    let told = requests[1]["messages"].to_string();
    assert!(told.contains("refused") && told.contains("needs a person"), "{told}");
    // Routine work in the same session still ran.
    assert!(s.project.join("build/x").exists());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("switched up") && stdout.contains("careful check"), "{stdout}");
}

#[test]
fn auto_mode_picks_per_request_and_an_explicit_mode_wins() {
    let s = Sandbox::new("auto-mode");
    let answer = || serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
    let (stdout, stderr, requests) = scripted_run(&s, &["--stats", "plan the database migration"], vec![answer()]);
    assert!(stdout.contains("◇ careful") && stdout.contains("mentions a migration and data"), "{stdout}");
    assert!(stderr.contains("\"mode_reason\":\"mentions a migration and data\""), "{stderr}");
    assert_eq!(requests.len(), 1, "a plan with no changes gets no checker");
    let (stdout, _, _) = scripted_run(&s, &["sketch a quick landing page"], vec![answer()]);
    assert!(stdout.contains("◇ vibe"), "{stdout}");
    let (stdout, _, _) = scripted_run(&s, &["--mode", "standard", "plan the database migration"], vec![answer()]);
    assert!(!stdout.contains("◇ careful") && !stdout.contains("auto ·"), "{stdout}");
    // Picking a mode never changes permissions.
    let out = s.repl(&["/mode auto", "/permissions check git push origin main", "/mode"]);
    assert!(out.contains("? asks first") && out.contains("execution auto"), "{out}");
}

#[test]
fn blank_directory_uses_default_and_one_empty_reply_recovers() {
    let s = Sandbox::new("empty-recovery");
    std::fs::write(s.project.join("hello.txt"), "hello").unwrap();
    let (stdout, _, requests) = scripted_run(
        &s,
        &["--mode", "vibe", "inspect files"],
        vec![tool_reply("list_files", serde_json::json!({"path":""})), text_reply(""), text_reply("Found hello.txt")],
    );
    assert!(stdout.contains("Found hello.txt"));
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["content"].as_str().is_some_and(|s| s.contains("hello.txt"))));
}

#[test]
fn two_empty_replies_fail_without_an_unbounded_retry_loop() {
    let s = Sandbox::new("empty-fail");
    let (url, server) = scripted_endpoint(vec![text_reply(""), text_reply("")]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).args(["--mode", "vibe", "inspect files"]).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("two empty replies"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn swarm_overlaps_three_workers_and_keeps_them_read_only() {
    use serde_json::json;
    use std::io::{BufRead, BufReader, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    let s = Sandbox::new("parallel-swarm");
    for i in 0..3 {
        std::fs::write(s.project.join(format!("file{i}.txt")), format!("FACT-{i}")).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let gate = Arc::new((Mutex::new(0usize), Condvar::new()));
    let gate_server = gate.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_server = requests.clone();
    let server = std::thread::spawn(move || {
        let start = Instant::now();
        let mut handlers = Vec::new();
        // Four lead requests and three requests per worker.
        while requests_server.load(Ordering::SeqCst) < 13 {
            let (mut stream, _) = match listener.accept() {
                Ok(p) => p,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(start.elapsed() < Duration::from_secs(120), "missing swarm request");
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            let gate = gate_server.clone();
            let requests = requests_server.clone();
            handlers.push(std::thread::spawn(move || {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut size = 0;
                let mut headers = 0;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        // A client may open a speculative idle connection.
                        Ok(0) if headers == 0 => return,
                        Err(e) if headers == 0 && matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return,
                        Ok(0) => panic!("incomplete request headers"),
                        Err(e) => panic!("{e}"),
                        Ok(_) => headers += 1,
                    }
                    if line == "\r\n" { break; }
                    if let Some((key, val)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") { size = val.trim().parse().unwrap(); }
                    }
                }
                let mut bytes = vec![0; size]; reader.read_exact(&mut bytes).unwrap();
                let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                requests.fetch_add(1, Ordering::SeqCst);
                let messages = body["messages"].as_array().unwrap();
                let prompt = messages.iter().rev().find(|m| m["role"] == "user").unwrap()["content"].as_str().unwrap();
                let steps = messages.iter().filter(|m| m["role"] == "tool").count();
                let reply = if let Some(i) = prompt.strip_prefix("WORKER-").and_then(|p| p.chars().next()).and_then(|c| c.to_digit(10)) {
                    assert!(!body["tools"].to_string().contains("\"write_file\""), "write tool exposed to worker");
                    match steps {
                        0 => {
                            let (lock, cv) = &*gate;
                            let mut entered = lock.lock().unwrap(); *entered += 1; cv.notify_all();
                            let (entered, _) = cv.wait_timeout_while(entered, Duration::from_secs(120), |n| *n < 3).unwrap();
                            assert_eq!(*entered, 3, "workers did not overlap");
                            // Even a forged write tool call must be denied by policy.
                            tool_reply("write_file", json!({"path":"forbidden.txt","content":"worker wrote"}))
                        }
                        1 => {
                            assert!(messages.last().unwrap()["content"].as_str().unwrap().contains("denied"));
                            tool_reply("read_file", json!({"path":format!("file{i}.txt")}))
                        }
                        _ => {
                            assert!(messages.last().unwrap()["content"].as_str().unwrap().contains(&format!("FACT-{i}")));
                            text_reply(&format!("FACT-{i}"))
                        }
                    }
                } else {
                    match steps {
                        0 => tool_reply("swarm", json!({"tasks": (0..3).map(|i| json!({"description":format!("slice-{i}"),"prompt":format!("WORKER-{i}: inspect file{i}.txt")})).collect::<Vec<_>>()})),
                        1 => {
                            let report = messages.last().unwrap()["content"].as_str().unwrap();
                            assert_eq!(report.matches("(done)").count(), 3);
                            assert!((0..3).all(|i| report.contains(&format!("FACT-{i}"))));
                            tool_reply("write_file", json!({"path":"report.md","content":report}))
                        }
                        2 => tool_reply("bash", json!({"command":"test -s report.md && printf verified > proof.txt"})),
                        _ => text_reply("Done and verified."),
                    }
                };
                let data = format!("data: {reply}\n\ndata: [DONE]\n\n");
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",data.len(),data).unwrap();
            }));
        }
        for h in handlers {
            h.join().unwrap();
        }
    });
    let child = s
        .cmd()
        .env("RUSTY_BASE_URL", endpoint)
        .args(["--yolo", "--mode", "standard", "--agents", "swarm", "inspect three slices with a swarm"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait(child, Duration::from_secs(20));
    server.join().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(*gate.0.lock().unwrap(), 3);
    assert_eq!(requests.load(Ordering::SeqCst), 13);
    assert!(!s.project.join("forbidden.txt").exists());
    assert_eq!(std::fs::read_to_string(s.project.join("proof.txt")).unwrap(), "verified");
}

/// A subagent's brief carries its scope and stopping point, and the lead
/// hears which of the worker's path:line references point at nothing.
#[test]
fn a_worker_gets_its_brief_and_the_lead_hears_about_bad_references() {
    use serde_json::json;
    let s = Sandbox::new("worker-brief");
    std::fs::write(s.project.join("cache.py"), "def load():\n    pass\n\ndef save(d):\n    write(d)\n").unwrap();
    let brief = json!({"description": "find the writer", "prompt": "Which function writes the cache?",
        "scope": "cache.py only", "done_when": "the function, as path:line"});
    let (_, _, requests) = scripted_run(
        &s,
        &["--yolo", "--memory", "off", "--mode", "standard", "--agents", "sub", "who writes the cache?"],
        vec![
            tool_reply("task", brief),
            text_reply(
                "References:\n- cache.py:4-5 save writes it\n- cache.py:40-44 a helper\n- store.py:2 the format",
            ),
            text_reply("save, in cache.py."),
        ],
    );
    assert_eq!(requests.len(), 3);
    let worker = requests[1]["messages"].to_string();
    assert!(
        worker.contains("Scope: cache.py only") && worker.contains("Done when: the function, as path:line"),
        "{worker}"
    );
    assert!(worker.contains("Start with References"), "{worker}");
    assert!(!requests[0]["tools"].to_string().contains("\"expect\":"), "only careful mode asks for expectations");
    let report = requests[2]["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(report.contains("2 of the 3 path:line references"), "{report}");
    assert!(
        report.contains("cache.py:40-44 (cache.py has 5 lines)") && report.contains("store.py:2 (no such file)"),
        "{report}"
    );
    assert!(!report.contains("cache.py:4-5 ("), "a good reference is not flagged: {report}");
}

#[test]
fn unchanged_call_loop_stalls_the_turn_and_three_stalls_end_the_goal() {
    use serde_json::json;
    for mode in ["careful", "standard", "vibe"] {
        let s = Sandbox::new(&format!("stalled-{mode}"));
        s.fake("probe", "printf unchanged");
        let calls: Vec<_> = (0..6)
            .map(|i| {
                let (name, args) = if i < 4 {
                    ("bash", if i % 2 == 0 { "{\"command\":\"probe\"}" } else { "{ \"command\" : \"probe\" }" })
                } else if i == 4 {
                    ("write_file", "{\"path\":\"must-not-write.txt\",\"content\":\"wrong\"}")
                } else {
                    ("goal_done", "{\"evidence\":\"done\"}")
                };
                json!({"index":i,"id":format!("call-{i}"),"type":"function","function":{"name":name,"arguments":args}})
            })
            .collect();
        let reply = json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}});
        let (url, server) = scripted_endpoint(vec![reply.clone(), reply.clone(), reply]);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--yolo", "--mode", mode, "--goal", "inspect", "--stats"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map(|c| wait(c, Duration::from_secs(20)))
            .unwrap();
        // An unfinished goal: neither success nor an error.
        assert_eq!(out.status.code(), Some(2), "{mode} must exit 2 for a stalled goal");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("stalled three turns in a row") && stderr.contains("\"goal\":\"open\""), "{stderr}");
        assert!(stderr.contains("\"goal_end\":\"stalled\""), "{stderr}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("four consecutive identical"));
        assert_eq!(s.calls().lines().count(), 12, "each stall should stop later tools in its batch");
        assert!(!s.project.join("must-not-write.txt").exists());
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3, "an open goal gets a fresh turn after a stall, and only three");
        assert!(requests[1]["messages"].to_string().contains("This turn was stopped"), "{mode}");
    }
}

#[test]
fn repeated_call_with_new_results_is_progress() {
    use serde_json::json;
    let s = Sandbox::new("changing-results");
    s.fake("probe", "wc -l < \"$RUSTY_TEST_CALLS\"");
    let calls: Vec<_> = (0..6).map(|i| json!({"index":i,"id":format!("call-{i}"),"type":"function","function":{"name":"bash","arguments":"{\"command\":\"probe\"}"}}))
        .chain(std::iter::once(json!({"index":6,"id":"done","type":"function","function":{"name":"goal_done","arguments":"{\"evidence\":\"six new observations\"}"}}))).collect();
    let reply = json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}});
    let (stdout, stderr, requests) =
        scripted_run(&s, &["--yolo", "--mode", "standard", "--goal", "inspect", "--stats"], vec![reply]);
    assert_eq!(requests.len(), 1);
    assert_eq!(s.calls().lines().count(), 6);
    assert!(stderr.contains("\"goal\":\"done\""), "{stderr}");
    assert!(!stdout.contains("returned the same result"), "new observations must not trigger repetition warnings");
}

#[test]
fn a_repeated_read_with_the_same_result_is_not_sent_again() {
    let s = Sandbox::new("collapse-reads");
    std::fs::write(s.project.join("policy.json"), "{\"actions\": [\"queue:send\"]}\n").unwrap();
    let read = || tool_reply("read_file", serde_json::json!({"path": "policy.json"}));
    let (_, _, requests) = scripted_run(
        &s,
        &["--yolo", "--mode", "standard", "check the policy"],
        vec![read(), read(), text_reply("done")],
    );
    let tools: Vec<String> = requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(tools.len(), 2);
    assert!(tools[0].contains("queue:send"), "{tools:?}");
    assert!(tools[1].starts_with("unchanged:") && !tools[1].contains("queue:send"), "{tools:?}");
}

#[test]
fn careful_goal_keeps_its_single_checker_budget_after_resume() {
    use serde_json::json;
    let s = Sandbox::new("resumed-checker");
    let done = tool_reply("goal_done", json!({"evidence":"checked"}));
    let mut partial = text_reply("fixes still in progress");
    partial["choices"][0]["finish_reason"] = json!("length");
    let (url, server) = scripted_endpoint(vec![done.clone(), text_reply("reviewed once; fix edge case"), partial]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        .args(["--mode", "careful", "--goal", "inspect", "--stats"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|c| wait(c, Duration::from_secs(20)))
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
    assert_eq!(server.join().unwrap().len(), 3);
    let (url, server) = scripted_endpoint(vec![done]);
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "careful", "--continue"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/goal resume\n/exit").unwrap();
    let out = wait(child, Duration::from_secs(20));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("goal done"));
    assert_eq!(server.join().unwrap().len(), 1, "resume must not purchase a second checker");
}

/// Anthropic's stream for one turn, from (event type, data) pairs.
fn anthropic_sse(events: &[serde_json::Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
}

#[test]
fn claude_models_use_the_messages_api_and_keep_their_thinking() {
    use serde_json::json;
    let s = Sandbox::new("anthropic");
    let first = anthropic_sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 50, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "list first"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "bash", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": \"printf from-bash\"}"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
        json!({"type": "message_stop"}),
    ]);
    let second = anthropic_sse(&[
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 80, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "all done"}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 3}}),
    ]);
    let (url, server) = scripted_sse(vec![first, second]);
    let out = s
        .cmd()
        .env("ANTHROPIC_API_KEY", "offline-anthropic-key")
        .env("ANTHROPIC_BASE_URL", url.trim_end_matches("/v1"))
        .args(["--yolo", "--model", "claude-opus-5-5", "--stats", "list the files"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("all done"), "{stdout}");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let (headers, body) = &requests[0];
    assert!(headers.contains("x-api-key: offline-anthropic-key") && headers.contains("anthropic-version: 2023-06-01"));
    assert!(!headers.contains("authorization:"), "Anthropic keys never go in a bearer header");
    assert!(!headers.contains("anthropic-beta"), "betas are only sent to Anthropic's own API");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(body["system"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(body["tools"].as_array().unwrap().iter().filter(|t| t["name"] == "bash").count(), 1);
    let history = &requests[1].1["messages"];
    let turn = history.as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(turn["content"][0]["signature"], "sig-1", "thinking goes back unchanged");
    let result = history.as_array().unwrap().last().unwrap();
    assert_eq!(result["content"][0]["tool_use_id"], "toolu_1");
    assert!(result["content"][0]["content"].as_str().unwrap().contains("from-bash"));
    let stats = String::from_utf8_lossy(&out.stderr);
    assert!(stats.contains("\"prompt\":130"), "{stats}");
}

#[test]
fn gpt_models_use_openai_parameters() {
    let s = Sandbox::new("openai");
    let bodies = vec![format!("data: {}\n\ndata: [DONE]\n\n", text_reply("hello from gpt"))];
    let (url, server) = scripted_sse(bodies);
    let out = s
        .cmd()
        .env_remove("NVIDIA_API_KEY")
        .env("OPENAI_API_KEY", "offline-openai-key")
        .env("OPENAI_BASE_URL", &url)
        .args(["--model", "gpt-5", "--mode", "careful", "say hello"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello from gpt"));
    let requests = server.join().unwrap();
    let (headers, body) = &requests[0];
    assert!(headers.contains("authorization: bearer offline-openai-key"));
    assert_eq!(body["model"], "gpt-5");
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("temperature").is_none() && body.get("max_tokens").is_none());
    assert!(body["max_completion_tokens"].as_u64().unwrap() >= 16384);
}

#[test]
fn a_model_without_its_key_says_which_key_to_set() {
    let s = Sandbox::new("missing-key");
    let out = s.cmd().args(["--model", "claude-opus-5-5", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("ANTHROPIC_API_KEY"), "{err}");
}

#[test]
fn with_only_an_anthropic_key_claude_is_the_default() {
    let s = Sandbox::new("default-claude");
    let mut child = s
        .cmd()
        .env_remove("NVIDIA_API_KEY")
        .env("ANTHROPIC_API_KEY", "offline-anthropic-key")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/model").unwrap();
    let out = wait(child, Duration::from_secs(20));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("claude-opus-5-5"), "{text}");
}

#[test]
fn doctor_checks_every_provider_without_showing_keys() {
    let s = Sandbox::new("doctor");
    let (nvidia, nvidia_server) = scripted_http(vec![(
        200,
        r#"{"data": [{"id": "nvidia/nemotron-3-super-120b-a12b"}, {"id": "z-ai/glm-5.3"}]}"#.into(),
    )]);
    let (anthropic, anthropic_server) = scripted_http(vec![(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#.into(),
    )]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", &nvidia)
        .env("ANTHROPIC_API_KEY", "sk-ant-secret-value")
        .env("ANTHROPIC_BASE_URL", anthropic.trim_end_matches("/v1"))
        .arg("--doctor")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "a failing provider fails the check: {text}");
    assert!(text.contains("✓ compatible") && text.contains("2 models"), "{text}");
    assert!(text.contains("✗ anthropic") && text.contains("401"), "{text}");
    assert!(text.contains("not set up · add OPENAI_API_KEY"), "{text}");
    assert!(text.contains("model nvidia/nemotron-3-super-120b-a12b → compatible") && text.contains("listed"), "{text}");
    assert!(text.contains("ANTHROPIC_API_KEY") && !text.contains("secret-value"), "names only, never values");
    assert_eq!(anthropic_server.join().unwrap().len(), 1, "one attempt, no retries");
    let (headers, _) = &nvidia_server.join().unwrap()[0];
    assert!(headers.starts_with("get /v1/models"), "{headers}");
}

/// A JWT with these claims and no real signature, enough for an expiry or an email.
fn fake_jwt(claims: serde_json::Value) -> String {
    fn b64(b: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut s = String::new();
        for chunk in b.chunks(3) {
            let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
            for i in 0..chunk.len() + 1 {
                s.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        s
    }
    format!("{}.{}.sig", b64(b"{\"alg\":\"none\"}"), b64(claims.to_string().as_bytes()))
}

#[test]
fn codex_sign_in_serves_gpt_models_and_refreshes_a_refused_token() {
    let s = Sandbox::new("codex-login");
    let codex_home = s.home.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let access = fake_jwt(serde_json::json!({"exp": now + 3600}));
    let id = fake_jwt(serde_json::json!({"email": "dev@example.com"}));
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({"auth_mode": "chatgpt", "OPENAI_API_KEY": null, "last_refresh": "2026-10-01T00:00:00Z",
            "tokens": {"id_token": id, "access_token": access, "refresh_token": "refresh-1", "account_id": "acct_1"}})
        .to_string(),
    )
    .unwrap();
    std::fs::write(codex_home.join("config.toml"), "model = \"gpt-5-codex-test\"\n").unwrap();

    let reply = [
        r#"{"type":"response.created","response":{}}"#.to_string(),
        r#"{"type":"response.output_text.delta","delta":"signed in fine"}"#.into(),
        r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"signed in fine"}]}}"#.into(),
        r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3}}}"#.into(),
    ]
    .iter()
    .map(|e| format!("data: {e}\n\n"))
    .collect::<String>();
    // The first call is refused; after a refresh the second one answers.
    let (codex, codex_server) = scripted_http(vec![(401, r#"{"detail":"token expired"}"#.into()), (200, reply)]);
    let (refresh, refresh_server) = scripted_http(vec![(
        200,
        serde_json::json!({"access_token": fake_jwt(serde_json::json!({"exp": now + 7200})), "refresh_token": "refresh-2", "id_token": fake_jwt(serde_json::json!({"email": "dev@example.com"}))}).to_string(),
    )]);
    let out = s
        .cmd()
        .env("CODEX_HOME", &codex_home)
        .env("RUSTY_CODEX_BASE_URL", &codex)
        .env("RUSTY_CODEX_REFRESH_URL", format!("{refresh}/oauth/token"))
        .env("RUSTY_STREAM_IDLE_SECS", "1")
        .env_remove("OPENAI_API_KEY")
        .args(["--model", "gpt-5", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}\n{err}");
    assert!(text.contains("signed in fine"), "{text}\n{err}");

    let requests = codex_server.join().unwrap();
    assert_eq!(requests.len(), 2);
    let (headers, body) = &requests[1];
    assert!(headers.starts_with("post /v1/codex/responses"), "{headers}");
    assert!(headers.contains("chatgpt-account-id: acct_1"), "{headers}");
    assert!(headers.contains("openai-beta: responses=experimental"), "{headers}");
    assert!(headers.contains("session_id: "), "the prompt cache is keyed on a session id: {headers}");
    assert!(!headers.contains("refresh-"), "the refresh token never travels as a header: {headers}");
    assert!(
        headers.contains("authorization: bearer ") && !headers.contains(&access.to_ascii_lowercase()),
        "retried with the new token: {headers}"
    );
    assert_eq!(body["model"], "gpt-5");
    assert_eq!(body["store"], false);
    assert!(
        body["messages"].is_null() && !body["instructions"].as_str().unwrap().is_empty(),
        "Responses API shape: {body}"
    );
    assert_eq!(body["input"].as_array().unwrap().last().unwrap()["content"][0]["text"], "hi");
    assert_eq!(body["tools"][0]["type"], "function");
    assert!(body["tools"][0]["name"].is_string(), "{}", body["tools"][0]);

    let refreshes = refresh_server.join().unwrap();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0].1["grant_type"], "refresh_token");
    assert_eq!(refreshes[0].1["refresh_token"], "refresh-1");
    assert!(refreshes[0].1["client_id"].as_str().unwrap().starts_with("app_"));
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(codex_home.join("auth.json")).unwrap()).unwrap();
    assert_eq!(saved["tokens"]["refresh_token"], "refresh-2", "the refreshed tokens are saved for Codex too");
    assert_ne!(saved["tokens"]["access_token"], access);
    assert_eq!(saved["auth_mode"], "chatgpt", "Codex's own fields stay");
    assert_ne!(saved["last_refresh"], "2026-10-01T00:00:00Z");
    assert!(!text.contains("refresh-") && !err.contains("refresh-"), "tokens are never printed");
}

#[test]
fn codex_eof_retries_before_text_but_not_after_text() {
    let s = Sandbox::new("codex-eof-before-text");
    let codex_home = s.home.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    let access = fake_jwt(serde_json::json!({"exp": rusty::unix_now() + 3600}));
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({"auth_mode":"chatgpt","tokens":{"access_token":access,"refresh_token":"fixture-refresh","account_id":"acct_fixture"}}).to_string(),
    )
    .unwrap();

    let incomplete = "data: {\"type\":\"response.created\",\"response\":{}}\n\n".to_owned();
    let completed = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"recovered answer\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":2,\"output_tokens\":2}}}\n\n"
    )
    .to_owned();
    let (codex, server) = scripted_http(vec![(200, incomplete), (200, completed)]);
    let out = s
        .cmd()
        .env("CODEX_HOME", &codex_home)
        .env("RUSTY_CODEX_BASE_URL", &codex)
        .env_remove("OPENAI_API_KEY")
        .args(["--model", "gpt-5-codex-test", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("recovered answer"));
    assert_eq!(server.join().unwrap().len(), 2, "an EOF before answer text is retried");

    let s = Sandbox::new("codex-eof-after-text");
    let codex_home = s.home.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    let access = fake_jwt(serde_json::json!({"exp": rusty::unix_now() + 3600}));
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({"auth_mode":"chatgpt","tokens":{"access_token":access,"refresh_token":"fixture-refresh","account_id":"acct_fixture"}}).to_string(),
    )
    .unwrap();
    let partial = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer\"}\n\n".to_owned();
    let (codex, server) = scripted_http(vec![(200, partial)]);
    let out = s
        .cmd()
        .env("CODEX_HOME", &codex_home)
        .env("RUSTY_CODEX_BASE_URL", &codex)
        .env_remove("OPENAI_API_KEY")
        .args(["--model", "gpt-5-codex-test", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success(), "a partial answer without a terminal event is incomplete");
    assert!(String::from_utf8_lossy(&out.stdout).contains("partial answer"));
    assert_eq!(server.join().unwrap().len(), 1, "never retry after answer text was shown");
}

#[test]
fn codex_sign_in_is_the_default_when_nothing_else_is_set_up() {
    let s = Sandbox::new("codex-default");
    let codex_home = s.home.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({"auth_mode": "chatgpt", "tokens": {"access_token": "a", "refresh_token": "r", "account_id": "acct_9"}}).to_string(),
    )
    .unwrap();
    std::fs::write(codex_home.join("config.toml"), "# comment\nmodel = \"gpt-5-codex-test\"\n").unwrap();
    let out = s.cmd().env("CODEX_HOME", &codex_home).env_remove("NVIDIA_API_KEY").arg("--doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("• codex") && text.contains("chatgpt sign-in") && text.contains("not checked"), "{text}");
    assert!(!text.contains("✓ codex"), "saved credentials are not a live connection check: {text}");
    assert!(text.contains("model gpt-5-codex-test → codex"), "Codex's own model is the default: {text}");
    assert!(text.contains("not set up · add NVIDIA_API_KEY"), "{text}");
    assert!(!text.contains("acct_9"), "{text}");

    // With no model configured, the catalog Codex saved picks the default and
    // fills --list-models; hidden entries stay out.
    std::fs::write(codex_home.join("config.toml"), "# nothing chosen\n").unwrap();
    std::fs::write(
        codex_home.join("models_cache.json"),
        serde_json::json!({"models": [
            {"slug": "gpt-6-beta", "visibility": "list", "priority": 2},
            {"slug": "gpt-secret", "visibility": "hide", "priority": 1},
            {"slug": "gpt-6-alpha", "visibility": "list", "priority": 1},
        ]})
        .to_string(),
    )
    .unwrap();
    let out = s.cmd().env("CODEX_HOME", &codex_home).env_remove("NVIDIA_API_KEY").arg("--doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("model gpt-6-alpha → codex") && text.contains("not checked"), "{text}");
    let out =
        s.cmd().env("CODEX_HOME", &codex_home).env_remove("NVIDIA_API_KEY").arg("--list-models").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("gpt-6-alpha") && text.contains("gpt-6-beta") && !text.contains("gpt-secret"), "{text}");

    let out = s.cmd().env_remove("NVIDIA_API_KEY").arg("--doctor").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("not set up · run `codex login`"), "{text}");
}

#[test]
fn an_empty_base_url_means_the_default() {
    let s = Sandbox::new("empty-base");
    let out = s.cmd().env("RUSTY_BASE_URL", "").args(["--model", "gpt-5", "hi"]).stdin(Stdio::null()).output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("OPENAI_API_KEY is not set"), "gpt-5 must not go to an empty endpoint: {err}");
}

#[test]
fn switching_to_a_model_without_its_key_warns_at_once() {
    let s = Sandbox::new("model-warning");
    let out = s.repl(&["/model gpt-5", "/model openai/gpt-oss-20b"]);
    assert!(out.contains("model gpt-5 · openai needs OPENAI_API_KEY"), "{out}");
    assert!(out.contains("model openai/gpt-oss-20b · compatible"), "{out}");
}

#[test]
fn a_long_rate_limit_is_ridden_out() {
    let s = Sandbox::new("rate-limit");
    let mut responses: Vec<(u16, String)> = (0..8).map(|_| (429, r#"{"error":"slow down"}"#.to_string())).collect();
    responses.push((200, format!("data: {}\n\ndata: [DONE]\n\n", text_reply("made it"))));
    let (url, server) = scripted_http(responses);
    let out = s.cmd().env("RUSTY_BASE_URL", url).arg("hi").stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("made it"));
    assert_eq!(server.join().unwrap().len(), 9, "eight rate limits, then the answer");
}

#[test]
fn retries_stop_at_the_budget() {
    let s = Sandbox::new("retry-budget");
    let (url, server) = scripted_http(vec![(429, r#"{"error":"slow down"}"#.into())]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_RETRY_SECS", "0")
        .arg("hi")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("giving up after 1 attempts") && err.contains("429") && err.contains("slow down"), "{err}");
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn a_refused_key_stops_after_two_attempts() {
    let s = Sandbox::new("refused-key");
    let refused = (401, r#"{"error":"bad key"}"#.to_string());
    let (url, server) = scripted_http(vec![refused.clone(), refused]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .arg("--trajectory")
        .arg(s.home.join("refused.json"))
        .arg("hi")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(err.contains("giving up after 2 attempts") && err.contains("401") && err.contains("bad key"), "{err}");
    assert_eq!(server.join().unwrap().len(), 2);
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("refused.json")).unwrap()).unwrap();
    // One round-one backoff (2 to 2.5s) separates the two attempts; anything
    // longer means auth failures are waiting out the retry budget.
    let waited = trace["model_budget"]["retry_wait_seconds"].as_f64().unwrap();
    assert!(waited < 10.0, "auth failures don't wait out the budget: waited {waited}s");
}

#[test]
fn an_overload_inside_the_stream_is_retried() {
    let s = Sandbox::new("stream-overload");
    let overloaded =
        r#"data: {"error":{"code":503,"message":"Service temporarily overloaded","type":"service_unavailable"}}"#;
    let (url, server) = scripted_sse(vec![
        format!("{overloaded}\n\n"),
        format!("data: {}\n\ndata: [DONE]\n\n", text_reply("reviewed it")),
    ]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).arg("review this").stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("reviewed it"));
    assert_eq!(server.join().unwrap().len(), 2, "the overloaded stream was retried once");
}

#[test]
fn a_stream_that_goes_quiet_is_retried_instead_of_hanging() {
    use std::io::{Read, Write};
    let s = Sandbox::new("stream-stall");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let answer = format!("data: {}\n\ndata: [DONE]\n\n", text_reply("still here"));
        let mut seen = 0;
        let mut first_closed = false;
        for body in ["", answer.as_str()] {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
            sync::request(&mut stream);
            seen += 1;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            stream.flush().unwrap();
            if body.is_empty() {
                // Headers arrived, then nothing: longer than the idle limit below.
                let mut byte = [0u8; 1];
                first_closed = matches!(stream.read(&mut byte), Ok(0));
            } else {
                stream.write_all(body.as_bytes()).unwrap();
            }
        }
        (seen, first_closed)
    });
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_STREAM_IDLE_SECS", "1")
        .arg("hi")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("still here"));
    let (requests, first_closed) = server.join().unwrap();
    assert_eq!(requests, 2, "the quiet stream was given up on and retried");
    assert!(first_closed, "the timed-out response was dropped before retrying");
}

#[test]
fn a_stream_can_run_longer_than_the_idle_limit_when_bytes_keep_arriving() {
    use std::io::Write;
    let s = Sandbox::new("stream-active-long");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        sync::request(&mut stream);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
        stream.flush().unwrap();
        let pieces = ["still ", "working ", "after ", "idle limit"];
        for piece in pieces {
            let event = serde_json::json!({"choices": [{"delta": {"content": piece}, "finish_reason": null}]});
            write!(stream, "data: {event}\n\n").unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(600));
        }
        let done = serde_json::json!({"choices": [{"delta": {}, "finish_reason": "stop"}]});
        write!(stream, "data: {done}\n\ndata: [DONE]\n\n").unwrap();
        stream.flush().unwrap();
    });
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_STREAM_IDLE_SECS", "1")
        .arg("hi")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("still working after idle limit"));
    server.join().unwrap();
}

#[test]
fn an_unbudgeted_stream_can_be_quiet_longer_than_the_blocking_default_timeout() {
    use std::io::Write;
    let s = Sandbox::new("stream-quiet-over-30");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        sync::request(&mut stream);
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(30_500));
        let event = serde_json::json!({"choices": [{"delta": {"content": "still alive"}, "finish_reason": "stop"}]});
        let _ = write!(stream, "data: {event}\n\ndata: [DONE]\n\n");
        let _ = stream.flush();
    });
    let child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_STREAM_IDLE_SECS", "40")
        .env("RUSTY_RETRY_SECS", "0")
        .arg("hi")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait(child, Duration::from_secs(45));
    server.join().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("still alive"));
}

#[test]
fn a_quiet_codex_stream_is_retried_too() {
    use std::io::{Read, Write};
    let s = Sandbox::new("codex-stall");
    let codex_home = s.home.join("codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    std::fs::write(
        codex_home.join("auth.json"),
        serde_json::json!({"auth_mode": "chatgpt", "tokens": {"id_token": fake_jwt(serde_json::json!({"email": "dev@example.com"})),
            "access_token": fake_jwt(serde_json::json!({"exp": now + 3600})), "refresh_token": "refresh-1", "account_id": "acct_1"}})
        .to_string(),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let answer = [
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.output_text.delta","delta":"still signed in"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"still signed in"}]}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":3}}}"#,
        ]
        .iter()
        .map(|e| format!("data: {e}\n\n"))
        .collect::<String>();
        // Headers then silence; then a stream that ends before response.completed; then the answer.
        let cut = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"hm\"}\n\n";
        let mut seen = 0;
        for body in ["", cut, answer.as_str()] {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
            sync::request(&mut stream);
            seen += 1;
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").unwrap();
            stream.flush().unwrap();
            if body.is_empty() {
                let mut byte = [0];
                assert!(matches!(stream.read(&mut byte), Ok(0)), "idle timeout must close the response");
            } else {
                stream.write_all(body.as_bytes()).unwrap();
            }
        }
        seen
    });
    let out = s
        .cmd()
        .env("CODEX_HOME", &codex_home)
        .env("RUSTY_CODEX_BASE_URL", url)
        .env("RUSTY_STREAM_IDLE_SECS", "1")
        .env_remove("OPENAI_API_KEY")
        .args(["--model", "gpt-5", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("still signed in"));
    assert_eq!(server.join().unwrap(), 3, "the quiet and the cut-off Codex streams were both retried");
    assert!(String::from_utf8_lossy(&out.stderr).contains("retrying in"), "a piped run still says what it waits for");
}

#[test]
fn an_error_after_text_has_streamed_is_not_retried() {
    let s = Sandbox::new("stream-late-error");
    let partial = serde_json::json!({"choices": [{"delta": {"content": "half an answer"}, "finish_reason": null}]});
    let body = format!("data: {partial}\n\ndata: {}\n\n", r#"{"error":{"code":503,"message":"overloaded"}}"#);
    let (url, server) = scripted_sse(vec![body]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).arg("hi").stdin(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("overloaded"));
    assert_eq!(server.join().unwrap().len(), 1, "a retry would repeat text the user already saw");
}

#[test]
fn a_drop_while_the_model_is_still_reasoning_is_retried() {
    let s = Sandbox::new("stream-reasoning-drop");
    let thinking =
        serde_json::json!({"choices": [{"delta": {"reasoning_content": "let me think"}, "finish_reason": null}]});
    let dropped = format!("data: {thinking}\n\ndata: {}\n\n", r#"{"error":{"code":503,"message":"overloaded"}}"#);
    let answer = serde_json::json!({"choices": [{"delta": {"content": "the answer"}, "finish_reason": "stop"}]});
    let (url, server) = scripted_sse(vec![dropped, format!("data: {answer}\n\ndata: [DONE]\n\n")]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).arg("hi").stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("the answer"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn announcing_completion_through_echo_is_redirected_then_stopped() {
    let s = Sandbox::new("echo-wrapup");
    let replies = (1..=5).map(|i| bash_reply(&format!("echo \"Task completed ({i}).\" && exit 0"))).collect();
    let (url, server) = scripted_endpoint(replies);
    let child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "fix it"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let out = wait(child, Duration::from_secs(30));
    let requests = server.join().unwrap();
    let last_tool = |r: &serde_json::Value| {
        r["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"].to_string()
    };
    assert!(last_tool(&requests[1]).contains("plain reply with no tool call"), "{}", last_tool(&requests[1]));
    assert_eq!(requests.len(), 5, "the fifth announcement ends the turn");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("five commands that only printed text"));
}

fn start_shell(command: &str, timeout: u64) -> serde_json::Value {
    tool_reply("bash_start", serde_json::json!({"command":command,"timeout_secs":timeout}))
}

fn wait_shell(seconds: u64) -> serde_json::Value {
    tool_reply("bash_wait", serde_json::json!({"id":"__owned_shell__","wait_secs":seconds}))
}

fn tool_batch(replies: Vec<serde_json::Value>) -> serde_json::Value {
    let calls: Vec<_> = replies
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let mut call = r["choices"][0]["delta"]["tool_calls"][0].clone();
            call["index"] = serde_json::json!(i);
            call["id"] = serde_json::json!(format!("batch-{i}"));
            call
        })
        .collect();
    serde_json::json!({"choices":[{"delta":{"tool_calls":calls},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}})
}

#[test]
fn registered_unchanged_waits_finish_without_disabling_the_stall_guard() {
    let s = Sandbox::new("owned-wait-progress");
    let replies = vec![
        start_shell("sleep 1; printf ready > marker", 5),
        tool_batch((0..6).map(|_| wait_shell(0)).collect()),
        wait_shell(3),
        done_reply(),
    ];
    let (out, trace, requests) = verified_run(&s, "test \"$(cat marker)\" = ready", replies, "standard", "yolo");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 4);
    assert_eq!(trace["activities"][0]["status"], "Exited");
    assert_eq!(trace["activities"][0]["exit_code"], 0);
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    let outputs: Vec<_> = trace["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .filter_map(|m| m["content"].as_str())
        .filter(|s| s.contains("\"status\":\"Running\""))
        .collect();
    assert!(outputs.len() >= 6);
    assert!(outputs.windows(2).all(|w| w[0] == w[1]), "identical live runtime results must be valid waits");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("four consecutive identical"));
    assert!(requests[1]["messages"].to_string().contains("Owned shell activities"));
}

#[test]
fn volatile_plan_and_activity_state_stays_after_the_history() {
    let s = Sandbox::new("volatile-prompt-state");
    let trajectory = s.project.join("trace.json");
    let plan = |text: &str| tool_reply("plan", serde_json::json!({"items":[{"text":text,"status":"in_progress"}]}));
    let (_, _, baseline) =
        scripted_run(&s, &["--memory", "off", "--mode", "standard", "baseline"], vec![text_reply("done")]);
    let (_, _, requests) = scripted_run(
        &s,
        &["--memory", "off", "--mode", "standard", "--trajectory", "trace.json", "inspect the project"],
        vec![
            plan("plan-alpha"),
            plan("plan-beta"),
            start_shell("sleep 30", 60),
            tool_reply("bash_cancel", serde_json::json!({"id":"__owned_shell__"})),
            text_reply("done"),
        ],
    );
    assert_eq!(requests.len(), 5);
    let system = |request: &serde_json::Value| request["messages"][0]["content"].as_str().unwrap().to_owned();
    let baseline_system = system(&baseline[0]);
    assert!(requests.iter().all(|request| system(request) == baseline_system));

    let tail = |request: &serde_json::Value| {
        request["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_owned()
    };
    assert!(tail(&requests[1]).contains("Current plan:") && tail(&requests[1]).contains("plan-alpha"));
    assert!(tail(&requests[2]).contains("plan-beta") && !tail(&requests[2]).contains("plan-alpha"));
    assert!(tail(&requests[3]).contains("Owned shell activities") && tail(&requests[3]).contains("sleep 30"));
    assert!(tail(&requests[4]).contains("plan-beta") && !tail(&requests[4]).contains("Owned shell activities"));

    let persisted: serde_json::Value = serde_json::from_slice(&std::fs::read(trajectory).unwrap()).unwrap();
    let messages = persisted["messages"].to_string();
    assert!(!messages.contains("Current plan:") && !messages.contains("Owned shell activities"));
}

#[test]
fn btw_receives_current_plan_after_main_history_is_compacted() {
    let s = Sandbox::new("aside-compacted-plan");
    let trajectory = s.project.join("trace.json");
    let long_prompt = "MAIN_HISTORY_SENTINEL ".repeat(1_500);
    let (url, server) = scripted_endpoint(vec![
        tool_reply("plan", serde_json::json!({"items":[{"text":"PLAN_AFTER_COMPACTION","status":"in_progress"}]})),
        say("main answer"),
        say("compacted summary without the plan"),
        say("aside answer"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off", "--trajectory"]).arg(&trajectory);
    let _ = s.repl_with(cmd, &[long_prompt.as_str(), "/compact", "/btw what is the current plan?", "/quit"]);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[2]["messages"].to_string().contains("MAIN_HISTORY_SENTINEL"));
    assert!(requests[3]["messages"].to_string().contains("Current plan:"));
    let plan_messages: Vec<_> = requests[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message.to_string().contains("PLAN_AFTER_COMPACTION"))
        .collect();
    assert_eq!(plan_messages.len(), 1, "{:#?}", requests[3]["messages"]);
    assert_eq!(plan_messages[0]["role"], "user");
    assert!(plan_messages[0]["content"].as_str().unwrap().contains("Current plan:"));
    assert!(requests[3]["tools"].is_null() || requests[3]["tools"].as_array().is_some_and(Vec::is_empty));

    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(trajectory).unwrap()).unwrap();
    assert!(!saved["messages"].to_string().contains("Current plan:"));
    assert!(saved["messages"].to_string().contains("MAIN_HISTORY_SENTINEL"));
    assert!(saved["compactions"].as_array().is_some_and(|events| !events.is_empty()));
}

#[test]
fn pending_activity_cannot_close_a_goal_or_spend_the_careful_review_completion_contract() {
    for mode in ["standard", "careful"] {
        let s = Sandbox::new(&format!("owned-premature-{mode}"));
        let (out, trace, requests) = verified_run(
            &s,
            "true",
            vec![start_shell("sleep 30; printf late > marker", 30), done_reply(), text_reply("still pending")],
            mode,
            "yolo",
        );
        assert!(!out.status.success());
        assert_eq!(requests.len(), 3, "a pending command must not purchase a reviewer");
        assert_eq!(trace["activities"][0]["status"], "Cancelled");
        assert_eq!(trace["verification"].as_array().unwrap().len(), 0);
        assert!(trace["messages"].to_string().contains("owned commands are still running"));
        assert!(!s.project.join("marker").exists());
        assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
    }
}

#[test]
fn owned_deadline_and_real_failure_cannot_supply_passing_evidence() {
    for (name, command, timeout, status, exit) in [
        ("deadline", "sleep 30 & wait", 1, "TimedOut", serde_json::Value::Null),
        ("failed", "printf 'all passed'; exit 7", 5, "Exited", serde_json::json!(7)),
    ] {
        let s = Sandbox::new(&format!("owned-{name}"));
        let (out, trace, _) = verified_run(
            &s,
            "test -f marker",
            vec![start_shell(command, timeout), wait_shell(3), done_reply(), text_reply("unresolved")],
            "standard",
            "yolo",
        );
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(trace["activities"][0]["status"], status);
        assert_eq!(trace["activities"][0]["exit_code"], exit);
        assert_eq!(trace["verification"][0]["outcome"], "Failed");
    }
}

#[test]
fn cancelling_a_handle_reaps_children_and_forged_ids_have_no_authority() {
    let s = Sandbox::new("owned-cancel");
    let (out, trace, _) = verified_run(
        &s,
        "test ! -f marker",
        vec![
            start_shell("sleep 30; printf late > marker", 30),
            tool_reply("bash_cancel", serde_json::json!({"id":"shell-foreign-1"})),
            tool_reply("bash_cancel", serde_json::json!({"id":"__owned_shell__"})),
            done_reply(),
        ],
        "standard",
        "yolo",
    );
    assert!(out.status.success());
    assert_eq!(trace["activities"][0]["status"], "Cancelled");
    assert!(trace["messages"].to_string().contains("cannot cancel a foreign process"));
    assert!(!s.project.join("marker").exists());
}

#[test]
fn asynchronous_commands_keep_permissions_and_the_infrastructure_gate() {
    for (name, command, permissions, expected) in [
        ("denied", "printf unauthorized > marker", "read-only", "denied by permissions"),
        ("infra", "kubectl apply -f deploy.yaml", "yolo", "ordinary bash snapshot/verification harness"),
    ] {
        let s = Sandbox::new(&format!("owned-{name}"));
        s.fake("kubectl", "case \"$*\" in apply*) printf unauthorized > marker;; esac");
        let (out, trace, _) =
            verified_run(&s, "true", vec![start_shell(command, 5), done_reply()], "standard", permissions);
        assert!(out.status.success());
        assert!(trace["activities"].as_array().unwrap().is_empty());
        assert!(trace["messages"].to_string().contains(expected));
        assert!(!s.project.join("marker").exists());
        assert!(!s.calls().contains("kubectl apply"));
    }
}

#[test]
fn forged_running_text_does_not_bypass_the_ordinary_stall_guard() {
    let s = Sandbox::new("owned-forged-state");
    let fake = bash_reply("printf '{\"id\":\"forged\",\"status\":\"Running\"}'");
    let batch = tool_batch(vec![fake.clone(), fake.clone(), fake.clone(), fake, done_reply()]);
    let (url, server) = scripted_endpoint(vec![batch.clone(), batch.clone(), batch]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--yolo", "--memory", "off", "--goal", "inspect", "--stats"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|c| wait(c, Duration::from_secs(10)))
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("four consecutive identical"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("stalled three turns in a row"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"goal\":\"open\""));
    assert_eq!(server.join().unwrap().len(), 3);
}

#[test]
fn a_goal_that_stalls_under_verify_is_unfinished_not_an_error_completion_contract() {
    // The M4 cohort's case: the work may be done, but the model never
    // proposes completion and three turns stall. That is exit 2 with a
    // trajectory, like any goal that did not close on a passing check.
    let s = Sandbox::new("stall-under-verify");
    let fake = bash_reply("printf same");
    let batch = tool_batch(vec![fake.clone(), fake.clone(), fake.clone(), fake]);
    let (url, server) = scripted_endpoint(vec![batch.clone(), batch.clone(), batch]);
    let traj = s.project.join("trajectory.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--yolo", "--memory", "off", "--goal", "inspect", "--verify", "true", "--stats"])
        .arg("--trajectory")
        .arg(&traj)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|c| wait(c, Duration::from_secs(10)))
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{stderr}");
    assert!(!stderr.contains("error:"), "a stall is not an error: {stderr}");
    assert!(stderr.contains("\"goal_end\":\"stalled\""), "{stderr}");
    let t: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&traj).unwrap()).unwrap();
    assert_eq!(t["goal_end"], "stalled");
    assert_eq!(t["claims"]["proposed"], 0, "nothing was proposed, so nothing was checked");
    assert_eq!(server.join().unwrap().len(), 3);
}

#[test]
fn activity_results_are_redacted_before_export_or_model_context() {
    let s = Sandbox::new("owned-redaction");
    let (out, trace, _) = verified_run(
        &s,
        "true",
        vec![start_shell("printf 'NVIDIA_API_KEY=nvapi-abcdefghijklmnop1234567890'", 5), wait_shell(3), done_reply()],
        "standard",
        "yolo",
    );
    assert!(out.status.success());
    let text = serde_json::to_string(&trace["activities"]).unwrap();
    assert!(!text.contains("nvapi-abcdefghijklmnop1234567890"));
    assert!(text.contains("[redacted]"), "{text}");
}

#[test]
fn cross_file_eval_graders_accept_oracles_and_reject_wrong_patches() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals");
    let index_good = "from normalize import key\ndef index(lines):\n    groups={}\n    for value in lines:\n        k=key(value)\n        if not k: continue\n        if k not in groups: groups[k]={\"key\":k,\"first\":value,\"count\":0}\n        groups[k][\"count\"]+=1\n    return [groups[k] for k in sorted(groups)]\n";
    let money_good = "from decimal import Decimal,ROUND_HALF_UP\ndef cents(value):\n    return int((Decimal(value)*100).quantize(Decimal(\"1\"),rounding=ROUND_HALF_UP))\n";
    let invoice_good = "from decimal import Decimal,ROUND_HALF_UP\nfrom money import cents\ndef total(lines,tax):\n    subtotal=sum(q*cents(p) for q,p in lines)\n    return int((Decimal(subtotal)*(1+Decimal(tax))).quantize(Decimal(\"1\"),rounding=ROUND_HALF_UP))\n";
    for task in ["verified-index", "verified-invoice"] {
        let s = Sandbox::new(&format!("grader-{task}"));
        let dir = root.join(task);
        for file in std::fs::read_dir(dir.join("files")).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), s.project.join(file.file_name())).unwrap();
        }
        let grade = || {
            Command::new("python3")
                .arg(dir.join("hidden.py"))
                .current_dir(&s.project)
                .output()
                .unwrap()
                .status
                .success()
        };
        assert!(!grade(), "broken seed must be red: {task}");
        let (first, second, good_first, good_second, wrong_first, wrong_second, public) = if task == "verified-index" {
            (
                "normalize.py",
                "indexer.py",
                "def key(value):\n    return value.strip().casefold()\n",
                index_good,
                "def key(value):\n    return value.strip().lower()\n",
                "def index(lines):\n    return []\n",
                "check_slow.py",
            )
        } else {
            (
                "money.py",
                "invoice.py",
                money_good,
                invoice_good,
                "def cents(value):\n    return round(float(value)*100)\n",
                "def total(lines,tax):\n    return 217\n",
                "check_public.py",
            )
        };
        std::fs::write(s.project.join(first), good_first).unwrap();
        std::fs::write(s.project.join(second), good_second).unwrap();
        assert!(grade(), "oracle must pass: {task}");
        std::fs::write(s.project.join(first), wrong_first).unwrap();
        assert!(!grade(), "plausible normalization/rounding error must fail: {task}");
        std::fs::write(s.project.join(first), good_first).unwrap();
        std::fs::write(s.project.join(second), wrong_second).unwrap();
        assert!(!grade(), "constant output must fail: {task}");
        std::fs::write(s.project.join(second), good_second).unwrap();
        std::fs::write(s.project.join(public), "print('everything passed')\n").unwrap();
        assert!(!grade(), "weakening protected public check violates task: {task}");
        assert!(!s.project.join("hidden.py").exists());
    }
}

#[test]
fn ctrl_c_cancels_and_reaps_an_owned_command_before_export() {
    let s = Sandbox::new("owned-interrupt");
    let (url, server) =
        scripted_endpoint(vec![start_shell("echo $$ > owner.pid; sleep 30 & wait", 30), wait_shell(30)]);
    let child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args([
            "--mode",
            "standard",
            "--agents",
            "off",
            "--memory",
            "off",
            "--yolo",
            "--goal",
            "wait for a command",
            "--verify",
            "true",
            "--stats",
            "--trajectory",
        ])
        .arg(s.home.join("interrupted.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(server.join().unwrap().len(), 2);
    sync::until("owned shell PID", || {
        std::fs::read_to_string(s.project.join("owner.pid")).ok().and_then(|s| s.trim().parse::<i32>().ok()).is_some()
    });
    let pid: i32 = std::fs::read_to_string(s.project.join("owner.pid")).unwrap().trim().parse().unwrap();
    let before = Instant::now();
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let out = wait(child, Duration::from_secs(60));
    assert_eq!(out.status.code(), Some(130), "{}", String::from_utf8_lossy(&out.stderr));
    // Generous for a loaded machine, yet well short of the 30s sleep: only
    // a working cancel can exit this soon.
    assert!(before.elapsed() < Duration::from_secs(15), "Ctrl-C waited for the command to finish");
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("interrupted.json")).unwrap()).unwrap();
    assert_eq!(trace["activities"][0]["status"], "Interrupted");
    assert_eq!(trace["goal"]["turns"], 1, "interrupted partial turns count as attempted");
    assert_eq!(trace["goal"]["run_turns"], 1);
    assert_eq!(trace["goal"]["stop_reason"], "interrupted");
    assert_eq!(trace["verification"].as_array().unwrap().len(), 0);
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "shell leader must be reaped");
}

#[test]
fn saved_activity_receipts_never_restore_a_live_handle_or_replay_work() {
    let s = Sandbox::new("owned-resume");
    let (_, trace, _) = verified_run(
        &s,
        "true",
        vec![
            start_shell("sleep 30; printf replayed > marker", 30),
            tool_reply("goal_done", serde_json::json!({"evidence":"need input","blocked":true})),
        ],
        "standard",
        "yolo",
    );
    let id = trace["activities"][0]["id"].as_str().unwrap();
    let (url, server) = scripted_endpoint(vec![
        tool_reply("bash_wait", serde_json::json!({"id":id,"wait_secs":0})),
        tool_reply("goal_done", serde_json::json!({"evidence":"old handle cannot be resumed","blocked":true})),
    ]);
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--agents", "off", "--memory", "off", "--yolo", "--continue", "--trajectory"])
        .arg(s.home.join("resumed.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/goal resume\n/exit").unwrap();
    let out = wait(child, Duration::from_secs(10));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["messages"].to_string().contains("No live handles are restored"));
    assert!(requests[1]["messages"].to_string().contains("unknown activity handle"));
    let resumed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("resumed.json")).unwrap()).unwrap();
    assert!(resumed["activities"].as_array().unwrap().is_empty());
    assert!(!s.project.join("marker").exists());
}

fn budget_run(
    s: &Sandbox,
    args: &[&str],
    responses: Vec<(u16, String)>,
) -> (Output, Vec<(String, serde_json::Value)>, serde_json::Value) {
    let (url, server) = scripted_http(responses);
    let trajectory = s.project.join("budget-trajectory.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_RETRY_SECS", "60")
        .env("RUSTY_MODE", "standard")
        .env("NVIDIA_API_KEY_2", "offline-test-key-2")
        .args(["--memory", "off", "--stats", "--trajectory"])
        .arg(&trajectory)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(15)))
        .unwrap();
    let requests = server.join().unwrap();
    let trace = serde_json::from_str(&std::fs::read_to_string(trajectory).unwrap()).unwrap();
    (out, requests, trace)
}
fn sse_reply(v: serde_json::Value) -> (u16, String) {
    (200, format!("data: {v}\n\ndata: [DONE]\n\n"))
}

#[test]
fn root_budget_counts_rotated_keys_and_stream_retries() {
    for stream_error in [false, true] {
        let s = Sandbox::new(if stream_error { "budget-stream-retry" } else { "budget-key-retry" });
        let response = if stream_error {
            sse_reply(serde_json::json!({"error":{"type":"rate_limit_error"}}))
        } else {
            (429, "rate limited".into())
        };
        let (out, requests, trace) =
            budget_run(&s, &["--max-requests", "2", "inspect"], vec![response.clone(), response]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("model budget exhausted"));
        assert_eq!(requests.len(), 2);
        let b = &trace["model_budget"];
        assert_eq!(b["requests"], 2);
        assert_eq!(b["unknown_usage_requests"], 2);
        assert_eq!(b["known_tokens"], 0);
        assert_eq!(b["active_requests"], 0);
        let reserved: u64 = requests
            .iter()
            .map(|(_, r)| serde_json::to_vec(r).unwrap().len() as u64 + r["max_tokens"].as_u64().unwrap())
            .sum();
        assert_eq!(b["charged_tokens"], reserved);
    }
}

#[test]
fn a_budget_ledger_spans_restarted_runs() {
    let s = Sandbox::new("budget-ledger");
    let ledger = s.home.join("episode.json");
    let run = |replies| {
        let (url, server) = scripted_endpoint(replies);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_BUDGET_LEDGER", &ledger)
            .args(["--yolo", "--memory", "off", "--max-requests", "2", "look around"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (out, server.join().unwrap())
    };
    let done = serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]});
    let (out, requests) = run(vec![done]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 1);
    // The restarted run has one request left, not two.
    let (out, requests) = run(vec![bash_reply("ls")]);
    assert_eq!(requests.len(), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("model budget exhausted"));
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&ledger).unwrap()).unwrap();
    assert_eq!(
        (saved["requests"].as_u64(), saved["attempts"].as_u64(), saved["runs"].as_u64()),
        (Some(2), Some(2), Some(2))
    );
    assert_eq!(saved["denied_requests"], 1, "{saved}");
}

#[test]
fn root_budget_refuses_before_first_post_and_validates_limits() {
    let s = Sandbox::new("budget-preflight");
    let (out, requests, trace) = budget_run(&s, &["--max-budget-tokens", "1", "inspect"], vec![]);
    assert!(!out.status.success());
    assert!(requests.is_empty());
    assert_eq!(trace["model_budget"]["requests"], 0);
    assert_eq!(trace["model_budget"]["charged_tokens"], 0);
    for flag in ["--max-requests", "--max-budget-tokens", "--budget-secs"] {
        let out = s.cmd().args([flag, "0", "inspect"]).output().unwrap();
        assert!(!out.status.success());
    }
}

#[test]
fn root_budget_retains_missing_or_partial_usage() {
    for usage in [
        serde_json::Value::Null,
        serde_json::json!({"prompt_tokens":1}),
        serde_json::json!({"prompt_tokens":-1,"completion_tokens":2}),
    ] {
        let s = Sandbox::new("budget-partial");
        let mut response = bash_reply("printf observed");
        response["usage"] = usage;
        let (out, requests, trace) = budget_run(&s, &["--max-requests", "1", "inspect"], vec![sse_reply(response)]);
        assert!(!out.status.success());
        assert_eq!(requests.len(), 1);
        let r = &requests[0].1;
        assert_eq!(
            trace["model_budget"]["charged_tokens"],
            serde_json::to_vec(r).unwrap().len() as u64 + r["max_tokens"].as_u64().unwrap()
        );
        assert_eq!(trace["model_budget"]["unknown_usage_requests"], 1);
        assert_eq!(trace["model_budget"]["known_tokens"], 0);
    }
}

#[test]
fn root_budget_settles_known_usage_and_verification_uses_no_model_request() {
    let s = Sandbox::new("budget-settle");
    let done = tool_reply("goal_done", serde_json::json!({"evidence":"checked"}));
    let (out, requests, trace) = budget_run(
        &s,
        &["--max-requests", "1", "--goal", "inspect", "--verify", "printf accepted"],
        vec![sse_reply(done)],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 1);
    assert!(trace["goal"]["status"].get("Done").is_some());
    assert_eq!(trace["goal_end"], "done");
    assert_eq!(trace["model_budget"]["charged_tokens"], 15);
    assert_eq!(trace["model_budget"]["known_tokens"], 15);
    assert_eq!(trace["model_budget"]["unknown_usage_requests"], 0);
    assert_eq!(trace["verification"][0]["exit_code"], 0);
}

#[test]
fn root_budget_reviewer_and_swarm_cannot_get_independent_allowances() {
    for review in [true, false] {
        let s = Sandbox::new(if review { "budget-review" } else { "budget-swarm" });
        let args = if review {
            vec!["--mode", "careful", "--max-requests", "1", "--goal", "inspect"]
        } else {
            vec!["--agents", "swarm", "--max-requests", "2", "inspect"]
        };
        let responses = if review {
            vec![sse_reply(tool_reply("goal_done", serde_json::json!({"evidence":"checked"})))]
        } else {
            vec![
                sse_reply(tool_reply(
                    "swarm",
                    serde_json::json!({"tasks":[{"description":"one","prompt":"inspect"},{"description":"two","prompt":"inspect"},{"description":"three","prompt":"inspect"}]}),
                )),
                sse_reply(text_reply("read-only findings")),
            ]
        };
        let (out, requests, trace) = budget_run(&s, &args, responses);
        assert!(!out.status.success());
        assert_eq!(requests.len(), if review { 1 } else { 2 });
        assert_eq!(trace["model_budget"]["requests"], requests.len());
        assert_eq!(trace["model_budget"]["by_role"]["lead"], 1);
        if !review {
            assert_eq!(trace["model_budget"]["by_role"]["worker"], 1);
        }
        assert!(trace["goal"]["status"].get("Done").is_none());
        if review {
            // The budget ran out inside the goal: an error, and it says so.
            assert_eq!(out.status.code(), Some(1));
            assert_eq!(trace["goal_end"], "error");
        } else {
            // Present and null: a missing key would mean an older build.
            assert!(trace.get("goal_end").is_some_and(serde_json::Value::is_null), "no goal ran");
        }
    }
}

#[test]
fn root_budget_clear_new_goal_and_compaction_keep_one_allowance() {
    use serde_json::json;
    let s = Sandbox::new("budget-clear");
    let response = text_reply(&"history ".repeat(1600));
    let (url, server) = scripted_endpoint(vec![response, text_reply("summary")]);
    let trajectory = s.project.join("trace.json");
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--max-requests", "2", "--memory", "off", "--mode", "standard", "--trajectory"])
        .arg(&trajectory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"inspect\n/compact\n/clear\n/goal inspect again\n/exit\n").unwrap();
    let out = wait(child, Duration::from_secs(15));
    assert!(out.status.success());
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["messages"].to_string().contains("Summarise the conversation"));
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(trajectory).unwrap()).unwrap();
    assert_eq!(trace["model_budget"]["by_role"], json!({"lead":1,"compaction":1}));
    assert_eq!(trace["model_budget"]["requests"], 2);
    assert!(trace["goal"]["status"].get("Done").is_none());
    assert!(String::from_utf8_lossy(&out.stdout).contains("model budget exhausted"));
}

#[test]
fn root_budget_error_cancels_owned_commands() {
    let s = Sandbox::new("budget-cancel");
    let start = tool_reply("bash_start", serde_json::json!({"command":"sleep 30","timeout":60}));
    let (out, requests, trace) = budget_run(&s, &["--max-requests", "1", "--goal", "inspect"], vec![sse_reply(start)]);
    assert!(!out.status.success());
    assert_eq!(requests.len(), 1);
    assert_eq!(trace["activities"][0]["status"], "Cancelled");
    assert!(trace["goal"]["status"].get("Done").is_none());
}

// The silent-response deadline and worker cleanup are tested with an injected
// clock in agent::budget_deadline_tests. Keep CLI flag-to-ledger wiring here.
#[test]
fn root_budget_seconds_reach_the_model_ledger() {
    let s = Sandbox::new("budget-seconds");
    let response = sse_reply(serde_json::json!({
        "choices":[{"delta":{"content":"inspected"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":5,"completion_tokens":1}
    }));
    let (out, requests, trace) = budget_run(&s, &["--budget-secs", "60", "inspect"], vec![response]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 1);
    assert_eq!(trace["model_budget"]["enabled"], true);
    assert_eq!(trace["model_budget"]["limits"]["seconds"], 60);
    assert_eq!(trace["model_budget"]["requests"], 1);
    assert_eq!(trace["model_budget"]["active_requests"], 0);
    assert_eq!(trace["model_budget"]["deadline_reached"], false);
}

#[test]
fn root_budget_anthropic_requires_final_usage_and_counts_cached_input() {
    for final_usage in [true, false] {
        let s = Sandbox::new("budget-anthropic");
        let mut events = [
            serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":5,"output_tokens":1}}}),
            serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"inspected"}}),
            serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        ];
        if final_usage {
            events[2]["usage"] = serde_json::json!({"output_tokens":20});
        }
        let body: String = events.iter().map(|v| format!("data: {v}\n\n")).collect();
        let (url, server) = scripted_sse(vec![body]);
        let tracepath = s.project.join("trace.json");
        let out = s
            .cmd()
            .env("ANTHROPIC_API_KEY", "offline-anthropic-key")
            .env("ANTHROPIC_BASE_URL", url)
            .args([
                "--model",
                "claude-test",
                "--max-requests",
                "1",
                "--mode",
                "standard",
                "--memory",
                "off",
                "--trajectory",
            ])
            .arg(&tracepath)
            .arg("inspect")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 1);
        let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(tracepath).unwrap()).unwrap();
        let b = &trace["model_budget"];
        if final_usage {
            assert_eq!(b["known_tokens"], 125);
            assert_eq!(b["charged_tokens"], 125);
            assert_eq!(b["unknown_usage_requests"], 0);
        } else {
            assert_eq!(b["known_tokens"], 0);
            assert_eq!(b["unknown_usage_requests"], 1);
        }
    }
}

#[test]
fn root_budget_openai_reserves_the_actual_completion_cap() {
    let s = Sandbox::new("budget-openai");
    let (url, server) = scripted_endpoint(vec![text_reply("inspected")]);
    let tracepath = s.project.join("trace.json");
    let out = s
        .cmd()
        .env("OPENAI_API_KEY", "offline-openai-key")
        .env("OPENAI_BASE_URL", url)
        .args(["--model", "gpt-5", "--max-requests", "1", "--mode", "careful", "--memory", "off", "--trajectory"])
        .arg(&tracepath)
        .arg("inspect")
        .output()
        .unwrap();
    assert!(out.status.success());
    let requests = server.join().unwrap();
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(tracepath).unwrap()).unwrap();
    let r = &requests[0];
    assert!(r.get("max_tokens").is_none());
    assert_eq!(
        trace["model_budget"]["charged_tokens"],
        serde_json::to_vec(r).unwrap().len() as u64 + r["max_completion_tokens"].as_u64().unwrap()
    );
}

#[test]
fn root_budget_keeps_usage_unknown_when_the_stream_later_fails() {
    let s = Sandbox::new("budget-failed-usage");
    let mut partial = text_reply("partial output");
    partial["usage"] = serde_json::json!({"prompt_tokens":10,"completion_tokens":5});
    let raw = format!("data: {}\n\ndata: {}\n\n", partial, serde_json::json!({"error":{"type":"api_error"}}));
    let (out, requests, trace) = budget_run(&s, &["--max-requests", "1", "inspect"], vec![(200, raw)]);
    assert!(!out.status.success());
    assert_eq!(requests.len(), 1);
    assert_eq!(trace["model_budget"]["unknown_usage_requests"], 1);
    assert_eq!(trace["model_budget"]["known_tokens"], 0);
}

#[test]
fn root_budget_overrun_rejects_completion_and_stops_new_dispatch_completion_contract() {
    let s = Sandbox::new("budget-overrun");
    let mut done = tool_reply("goal_done", serde_json::json!({"evidence":"checked"}));
    done["usage"] = serde_json::json!({"prompt_tokens":5_000_000,"completion_tokens":1});
    let (out, requests, trace) = budget_run(
        &s,
        &["--max-budget-tokens", "4000000", "--goal", "inspect", "--verify", "printf accepted"],
        vec![sse_reply(done)],
    );
    assert!(!out.status.success());
    assert_eq!(requests.len(), 1);
    assert_eq!(trace["model_budget"]["overrun"], true);
    assert!(trace["goal"]["status"].get("Done").is_none());
    assert!(trace["verification"].as_array().unwrap().is_empty());
}

#[test]
fn unknown_memory_level_names_the_levels() {
    let s = Sandbox::new("memory-level");
    let out = s.cmd().args(["--memory", "v2", "inspect"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unknown memory level `v2`") && err.contains("recall, learn, reflect, deep"), "{err}");
}

#[test]
fn root_budget_failed_expensive_review_cannot_be_skipped_on_resume() {
    let s = Sandbox::new("budget-review-resume");
    std::fs::write(s.home.join("settings.json"), r#"{"agents":{"sub_model":"claude-test"}}"#).unwrap();
    let (url, server) = scripted_endpoint(vec![tool_reply("goal_done", serde_json::json!({"evidence":"checked"}))]);
    let tracepath = s.project.join("trace.json");
    let mut child = s
        .cmd()
        .env("ANTHROPIC_API_KEY", "offline-anthropic-key")
        .env("ANTHROPIC_BASE_URL", &url)
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "careful", "--memory", "off", "--max-budget-tokens", "65000", "--trajectory"])
        .arg(&tracepath)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"/goal inspect\n/goal resume\n/clear\n/goal another goal\n/exit\n").unwrap();
    let out = wait(child, Duration::from_secs(15));
    assert!(out.status.success());
    assert_eq!(server.join().unwrap().len(), 1);
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(tracepath).unwrap()).unwrap();
    assert_eq!(trace["model_budget"]["halted"], true);
    assert_eq!(trace["model_budget"]["requests"], 1);
    assert!(trace["goal"]["status"].get("Done").is_none());
}

#[test]
fn goal_end_describes_the_last_goal_run_not_an_earlier_one() {
    // One process, two goals: the first closes, the second hits the budget.
    let s = Sandbox::new("goal-end-two-goals");
    let (url, server) = scripted_endpoint(vec![tool_reply("goal_done", serde_json::json!({"evidence":"checked"}))]);
    let tracepath = s.project.join("trace.json");
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--memory", "off", "--max-requests", "1", "--trajectory"])
        .arg(&tracepath)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"/goal first\n/goal second\n/exit\n").unwrap();
    let out = wait(child, Duration::from_secs(15));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 1);
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(tracepath).unwrap()).unwrap();
    assert_eq!(trace["goal_end"], "error", "the second goal's end, not the first goal's done");
}

#[test]
fn a_rate_limited_attempt_does_not_spend_the_request_budget() {
    let s = Sandbox::new("budget-rate-limited");
    let (out, requests, trace) = budget_run(
        &s,
        &["--max-requests", "1", "inspect"],
        vec![(429, r#"{"error":"slow down"}"#.into()), sse_reply(text_reply("looked"))],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(requests.len(), 2);
    assert_eq!(trace["model_budget"]["requests"], 1);
    assert_eq!(trace["model_budget"]["rate_limited"], 1);
    // A gateway sees both attempts and counts only the answered one.
    assert_eq!(
        (trace["model_budget"]["attempts"].as_u64(), trace["model_budget"]["http_ok"].as_u64()),
        (Some(2), Some(1))
    );
}

#[test]
fn a_review_that_quotes_a_budget_error_does_not_halt_the_run() {
    let s = Sandbox::new("careful-quote");
    let done = tool_reply("goal_done", serde_json::json!({"evidence": "checked"}));
    let (_, stderr, requests) = scripted_run(
        &s,
        &["--mode", "careful", "--goal", "check the workspace", "--stats"],
        vec![done.clone(), text_reply("no issues; the old log line 'model budget exhausted' is only test data"), done],
    );
    assert_eq!(requests.len(), 3);
    assert!(stderr.contains("\"goal\":\"done\""), "{stderr}");
}

#[test]
fn a_goal_turn_that_ends_with_a_running_command_continues_the_goal() {
    let s = Sandbox::new("owned-turn-end-goal");
    let (url, server) = scripted_endpoint(vec![
        start_shell("sleep 30; printf late > marker", 30),
        text_reply("started it; I'll check back"),
        bash_reply("echo checked"),
        done_reply(),
    ]);
    let path = s.home.join("turn-end.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "2")
        .args(["--agents", "off", "--memory", "off", "--mode", "standard", "--yolo", "--stats", "--goal", "run it"])
        .arg("--trajectory")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(15)))
        .unwrap();
    let requests = server.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(requests.len(), 4);
    assert!(requests[2]["messages"].to_string().contains("cancelled 1 owned command(s) at turn end"));
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(trace["activities"][0]["status"], "Cancelled");
    assert!(!s.project.join("marker").exists());
    assert!(stderr.contains("\"goal\":\"done\""), "{stderr}");
}

#[test]
fn trajectory_keeps_commands_that_compaction_summarised_away() {
    let s = Sandbox::new("trajectory-compaction");
    // No reported usage, so the context meter estimates the whole context.
    let replies = vec![
        unreported(bash_reply("echo first-marker; seq 100000 199999")),
        unreported(bash_reply("echo second")),
        text_reply("Ran two commands; the second printed filler."),
        text_reply("finished"),
    ];
    let (url, server) = scripted_endpoint(replies);
    let path = s.home.join("compacted.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        // Small enough that the first output fills most of what a request may use.
        .env("RUSTY_CONTEXT_TOKENS", "14000")
        .args(["--agents", "off", "--memory", "off", "--yolo", "--goal", "print some output", "--trajectory"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(15)))
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4, "{}", String::from_utf8_lossy(&out.stdout));
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let messages = trace["messages"].as_array().unwrap();
    assert!(trace["archived_messages"].as_u64().unwrap() > 0, "{}", String::from_utf8_lossy(&out.stdout));
    // The last request no longer carries the first command's output; the trajectory still does.
    assert!(!requests[3]["messages"].to_string().contains("first-marker\\n100000"));
    let first = messages
        .iter()
        .position(|m| m["content"].as_str().is_some_and(|c| c.contains("first-marker\n100000")))
        .unwrap();
    let summary = messages.iter().position(|m| m.to_string().contains("[Summary of the conversation so far]")).unwrap();
    assert!(first < summary);
    // The compaction is recorded where the summary sits, with what led to it.
    let c = &trace["compactions"][0];
    assert_eq!(c["trigger"], "level", "{c}");
    assert_eq!(c["boundary"].as_u64(), Some(summary as u64), "{c}");
    // Messages after the summary up to resumes_at were kept, not new; the first
    // message from resumes_at on came after the compaction.
    let resumes = c["resumes_at"].as_u64().unwrap() as usize;
    assert!(resumes > summary && resumes <= messages.len(), "{c}");
    assert!(messages[summary + 1..resumes].iter().any(|m| m.to_string().contains("echo second")), "{c}");
    assert_eq!(c["calls_since_last"], 2, "{c}");
    assert!(c["tokens_after"].as_u64() < c["tokens_before"].as_u64(), "{c}");
    assert!(!s.home.join("compacted.partial").exists());
}

#[test]
fn a_killed_run_still_leaves_a_trajectory() {
    let s = Sandbox::new("trajectory-killed");
    let (url, server) = scripted_endpoint(vec![bash_reply("echo step-one")]);
    let path = s.home.join("killed.json");
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--agents", "off", "--memory", "off", "--yolo", "--goal", "run two steps", "--trajectory"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < Duration::from_secs(120), "no trajectory before the kill");
        std::thread::sleep(Duration::from_millis(20));
    }
    child.kill().unwrap();
    child.wait().unwrap();
    server.join().unwrap();
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(trace["messages"].to_string().contains("step-one"));
    assert_eq!(trace["totals"]["requests"], 1);
}

#[test]
fn destructive_calls_run_unattended_only_with_the_opt_in_and_yolo() {
    for (case, permissions, opt_in, runs) in
        [("opted-in", "yolo", true, true), ("no-opt-in", "yolo", false, false), ("auto-mode", "auto", true, false)]
    {
        let s = Sandbox::new(&format!("unattended-{case}"));
        s.fake("kubectl", "printf 'kind-dev|default'");
        let (url, server) =
            scripted_endpoint(vec![bash_reply("kubectl drain node-2 --ignore-daemonsets"), text_reply("done")]);
        let mut cmd = s.cmd();
        cmd.env("RUSTY_BASE_URL", url).env("RUSTY_GOAL_MAX_TURNS", "1");
        if opt_in {
            cmd.env("RUSTY_ALLOW_DESTRUCTIVE", "1");
        }
        let out = cmd
            .args(["--agents", "off", "--memory", "off", "--permissions", permissions, "--goal", "retire node-2"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map(|child| wait(child, Duration::from_secs(15)))
            .unwrap();
        let requests = server.join().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(s.calls().contains("kubectl drain node-2"), runs, "{case}: {stdout}");
        assert_eq!(requests[1]["messages"].to_string().contains("nobody is watching"), !runs, "{case}");
        if runs {
            assert!(stdout.contains("allowed unattended by RUSTY_ALLOW_DESTRUCTIVE"), "{stdout}");
        }
    }
}

/// A stdio MCP server in shell: a read tool on the first page of its tool
/// list, and on the second a delete tool and one only its annotation marks
/// destructive. Each request line is logged, so a test can tell what reached
/// the server.
const FAKE_MCP: &str = r#"while IFS= read -r line; do
  echo "mcp> $line" >> "$RUSTY_TEST_CALLS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{\"tools\":{},\"resources\":{}}}}" ;;
    *'"tools/list"'*'"cursor":"p2"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"delete_flag\",\"description\":\"Delete a flag\",\"inputSchema\":{\"type\":\"object\"}},{\"name\":\"reset_flags\",\"description\":\"Reset all flags\",\"annotations\":{\"destructiveHint\":true}}]}}" ;;
    *'"tools/list"'*) echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"tools\":[{\"name\":\"get_flag\",\"description\":\"Read a flag\",\"inputSchema\":{\"type\":\"object\",\"properties\":{\"key\":{\"type\":\"string\"}}}}],\"nextCursor\":\"p2\"}}" ;;
    *'"tools/call"'*) echo "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}"; echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"flag checkout.engine = v2\"}]}}" ;;
  esac
done"#;

fn mcp_sandbox(name: &str) -> Sandbox {
    let s = Sandbox::new(name);
    s.fake("flags-mcp", FAKE_MCP);
    std::fs::write(
        s.project.join(".mcp.json"),
        r#"{"mcpServers": {"flags": {"command": "flags-mcp"}, "broken": {"command": "no-such-mcp-server"},
            "remote": {"url": "https://mcp.example.com/sse"}}}"#,
    )
    .unwrap();
    s
}

/// A secret store over MCP: its answer's `value` field is the secret.
const FAKE_VAULT_MCP: &str = r#"k="vault-live-$(printf 9f3a)2c1e7b"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"protocolVersion":"2025-06-18","capabilities":{}}}' ;;
    *'"tools/list"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"tools":[{"name":"secret_access","description":"Read a secret","inputSchema":{"type":"object"}}]}}' ;;
    *'"tools/call"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"content":[{"type":"text","text":"{\"version\": 1, \"value\": \"'$k'\"}"}]}}' ;;
  esac
done"#;

#[test]
fn a_secret_from_an_mcp_secret_store_is_usable_but_never_seen() {
    let s = Sandbox::new("mcp-vault");
    s.fake("vault-mcp", FAKE_VAULT_MCP);
    std::fs::write(s.project.join(".mcp.json"), r#"{"mcpServers": {"vault": {"command": "vault-mcp"}}}"#).unwrap();
    let key = format!("vault-live-{}2c1e7b", "9f3a");
    let (_, _, requests) = scripted_run(
        &s,
        &["--yolo", "--mode", "standard", "use the payments key"],
        vec![
            tool_reply("mcp__vault__secret_access", serde_json::json!({"name": "payments-key"})),
            bash_reply("printf %s \"$RUSTY_SECRET_1\" > used.txt && echo stored"),
            text_reply("done"),
        ],
    );
    let seen = requests[2]["messages"].to_string();
    assert!(!seen.contains(&key), "{seen}");
    assert!(seen.contains(r#"\"value\": \"${RUSTY_SECRET_1}\""#) && seen.contains("stored"), "{seen}");
    assert_eq!(std::fs::read_to_string(s.project.join("used.txt")).unwrap(), key);
}

#[test]
fn mcp_tools_are_offered_and_called() {
    let s = mcp_sandbox("mcp-call");
    let answer = serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]});
    let call = tool_reply("mcp__flags__get_flag", serde_json::json!({"key": "checkout.engine"}));
    let (url, server) = scripted_endpoint(vec![call, answer]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "read the checkout flag"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp") && stdout.contains("flags (3 tools)"), "{stdout}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mcp server `broken` unavailable"), "{stderr}");
    assert!(stderr.contains("mcp server `remote` unavailable: only stdio"), "{stderr}");
    let requests = server.join().unwrap();
    let offered = requests[0]["tools"].to_string();
    assert!(offered.contains("mcp__flags__get_flag") && offered.contains("[flags MCP] Read a flag"), "{offered}");
    let seen = requests[1]["messages"].to_string();
    assert!(seen.contains("flag checkout.engine = v2"), "{seen}");
    assert!(s.calls().contains(r#""name":"get_flag""#), "{}", s.calls());
}

#[test]
fn mcp_dispatch_outcomes_reach_trajectory_and_session_but_not_provider_requests() {
    for case in ["settled", "unknown", "not_started", "interrupted"] {
        let s = Sandbox::new(&format!("mcp-outcome-{case}"));
        s.fake(
            "outcome-mcp",
            r#"
while IFS= read -r line; do
  echo "mcp> $line" >> "$RUSTY_TEST_CALLS"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*) echo '{"jsonrpc":"2.0","id":'$id',"result":{"capabilities":{}}}' ;;
    *'"tools/list"'*)
      if test "$RUSTY_TEST_MCP_CASE" = not_started; then exec 0<&-; fi
      echo '{"jsonrpc":"2.0","id":'$id',"result":{"tools":[{"name":"get_flag","inputSchema":{"type":"object"}}]}}'
      if test "$RUSTY_TEST_MCP_CASE" = not_started; then exec sleep 60; fi ;;
    *'"tools/call"'*)
      if test "$RUSTY_TEST_MCP_CASE" = unknown; then exec sleep 60; fi
      if test "$RUSTY_TEST_MCP_CASE" = interrupted; then printf ready > "$RUSTY_TEST_MCP_READY"; exec sleep 60; fi
      echo '{"jsonrpc":"2.0","id":'$id',"error":{"code":-32000,"message":"fixture refusal"}}' ;;
  esac
done
"#,
        );
        std::fs::write(s.project.join(".mcp.json"), r#"{"mcpServers":{"outcome":{"command":"outcome-mcp"}}}"#).unwrap();
        let mut call = tool_reply("mcp__outcome__get_flag", serde_json::json!({}));
        let replies = if case == "interrupted" {
            let mut skipped = call["choices"][0]["delta"]["tool_calls"][0].clone();
            skipped["index"] = serde_json::json!(1);
            skipped["id"] = serde_json::json!("skipped");
            call["choices"][0]["delta"]["tool_calls"].as_array_mut().unwrap().push(skipped);
            vec![call]
        } else {
            vec![call, text_reply("The call's effect is not assumed successful.")]
        };
        let (url, server) = scripted_endpoint(replies);
        let ready = s.home.join("call-ready");
        let trace_path = s.home.join("outcome.json");
        let mut child = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_TEST_MCP_CASE", case)
            .env("RUSTY_TEST_MCP_READY", &ready)
            .env("RUSTY_MCP_TIMEOUT", if case == "interrupted" { "30" } else { "1" })
            .args(["--yolo", "--mode", "standard", "--agents", "off", "--memory", "off", "--trajectory"])
            .arg(&trace_path)
            .arg("inspect the flag")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if case == "interrupted" {
            let started = Instant::now();
            while !ready.exists() {
                if started.elapsed() > Duration::from_secs(15) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("MCP call did not reach the effect barrier");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
        }
        let out = wait(child, Duration::from_secs(30));
        let requests = server.join().unwrap();
        if case == "interrupted" {
            assert_eq!(out.status.code(), Some(130));
        } else {
            assert!(out.status.success(), "{case}: {}", String::from_utf8_lossy(&out.stderr));
        }
        assert_eq!(requests.len(), if case == "interrupted" { 1 } else { 2 });
        let expected = if case == "interrupted" { "unknown" } else { case };
        let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(&trace_path).unwrap()).unwrap();
        let result = trace["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(result["_mcp_outcome"], expected, "{case}: {trace}");
        assert_eq!(trace["mcp_outcomes"][expected], 1);
        assert!(requests.iter().all(|r| !r["messages"].to_string().contains("_mcp_outcome")));
        if case == "interrupted" {
            let results: Vec<_> =
                trace["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").collect();
            assert_eq!(results.len(), 2);
            assert_eq!(results[1]["_mcp_outcome"], "not_started");
            assert_eq!(trace["mcp_outcomes"]["not_started"], 1);
            assert_eq!(s.calls().matches("tools/call").count(), 1, "skipped call must not reach MCP");
        }
        let session_path = std::fs::read_dir(sessions_dir(&s))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|ext| ext == "json"))
            .unwrap();
        let session: serde_json::Value = serde_json::from_slice(&std::fs::read(session_path).unwrap()).unwrap();
        let saved = session["history"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(saved["_mcp_outcome"], expected);
        if case == "not_started" {
            assert!(!s.calls().contains("tools/call"), "{}", s.calls());
        }
        if case == "unknown" {
            assert!(result["content"].as_str().unwrap().starts_with("MCP outcome unknown:"));
        }
    }
}

#[test]
fn ctrl_c_during_mcp_startup_is_interrupted_and_reaps_the_server() {
    let s = Sandbox::new("mcp-startup-interrupted");
    let ready = s.home.join("mcp-ready");
    s.fake(
        "waiting-mcp",
        r#"IFS= read -r line
printf '%s' "$$" > "$RUSTY_TEST_MCP_READY.next"
mv "$RUSTY_TEST_MCP_READY.next" "$RUSTY_TEST_MCP_READY"
exec sleep 60"#,
    );
    std::fs::write(
        s.project.join(".mcp.json"),
        r#"{"mcpServers":{"required":{"command":"waiting-mcp","required":true}}}"#,
    )
    .unwrap();
    let mut child = s
        .cmd()
        .env("RUSTY_TEST_MCP_READY", &ready)
        .arg("--mcp-check")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let started = Instant::now();
    while !ready.exists() {
        if started.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("MCP server did not reach initialize");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let server_pid: i32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let out = wait(child, Duration::from_secs(15));
    assert_eq!(out.status.code(), Some(130), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["servers"][0]["reason"], "interrupted");
    assert_eq!(report["servers"][0]["status"], "interrupted");
    assert_eq!(unsafe { libc::kill(server_pid, 0) }, -1, "startup server was not reaped");
}

#[test]
fn unattended_mcp_deletes_are_refused_before_reaching_the_server() {
    // `reset_flags` is a delete only by its annotation.
    for tool in ["delete_flag", "reset_flags"] {
        let s = mcp_sandbox("mcp-delete");
        let answer = serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
        let call = tool_reply(&format!("mcp__flags__{tool}"), serde_json::json!({}));
        let (url, server) = scripted_endpoint(vec![call, answer]);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--yolo", "delete the flag"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let seen = server.join().unwrap()[1]["messages"].to_string();
        assert!(seen.contains("nobody is watching"), "{tool}: {seen}");
        assert!(!s.calls().contains(&format!("\"name\":\"{tool}\"")), "{}", s.calls());
    }
}

#[test]
fn required_mcp_servers_and_tools_fail_closed() {
    let s = mcp_sandbox("mcp-required");
    let check = || s.cmd().arg("--mcp-check").stdin(Stdio::null()).output().unwrap();
    let out = check();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let flags = &report["servers"].as_array().unwrap().iter().find(|r| r["server"] == "flags").unwrap();
    assert_eq!((flags["status"].as_str(), flags["tools"].as_u64()), (Some("ok"), Some(3)), "{report}");
    assert_eq!(report["ok"], true, "optional servers that fail don't block: {report}");
    assert_eq!(flags["ignored"], serde_json::json!(["resources"]), "{report}");

    let reason = |name: &str| {
        report["servers"].as_array().unwrap().iter().find(|r| r["server"] == name).unwrap()["reason"].clone()
    };
    assert_eq!((reason("broken"), reason("remote")), ("start_failed".into(), "unsupported_transport".into()));
    assert!(flags.get("reason").is_none(), "{report}");

    for (config, why) in [
        (
            r#"{"mcpServers": {"flags": {"command": "flags-mcp", "requiredTools": ["get_flag", "set_flag"]}}}"#,
            "\"reason\":\"missing_tools\"",
        ),
        (
            r#"{"mcpServers": {"gone": {"command": "no-such-mcp-server", "required": true}}}"#,
            "\"reason\":\"start_failed\"",
        ),
        (r#"{"mcpServers": {"mute": {"command": "true", "required": true}}}"#, "\"reason\":\"handshake_failed\""),
        (
            r#"{"mcpServers": {"remote": {"url": "https://mcp.example.com/sse", "required": true}}}"#,
            "\"reason\":\"unsupported_transport\"",
        ),
        (r#"{"mcpServers": {"odd": {"command": 7, "required": true}}}"#, "\"reason\":\"bad_config\""),
        (
            r#"{"mcpServers": {"sse": {"type": "sse", "command": "flags-mcp", "required": true}}}"#,
            "\"reason\":\"unsupported_transport\"",
        ),
    ] {
        std::fs::write(s.project.join(".mcp.json"), config).unwrap();
        let out = check();
        assert_eq!(out.status.code(), Some(1), "{config}");
        let report = String::from_utf8_lossy(&out.stdout);
        assert!(report.contains(r#""ok":false"#) && report.contains(why), "{report}");
        let out = s.cmd().env("RUSTY_BASE_URL", "http://127.0.0.1:9").args(["--yolo", "hi"]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && stderr.contains("required MCP server"), "{stderr}");
    }
}

fn swarm_call(jobs: usize) -> serde_json::Value {
    let tasks: Vec<_> = (0..jobs)
        .map(|i| serde_json::json!({"description": format!("slice {i}"), "prompt": format!("inspect slice {i}")}))
        .collect();
    tool_reply("swarm", serde_json::json!({ "tasks": tasks }))
}

fn say(text: &str) -> serde_json::Value {
    serde_json::json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}})
}

#[test]
fn swarm_workers_share_findings_and_the_lead_gets_them_all() {
    let s = Sandbox::new("swarm-board");
    let share = tool_reply("share", serde_json::json!({"finding": "root cause at api.rs:42"}));
    // One worker at a time, so the order is fixed: lead, worker 0 (shares, then
    // reports), worker 1 (sees the note, reports), lead again.
    let replies = vec![swarm_call(2), share, say("report zero"), say("report one"), say("done")];
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--agents", "swarm", "--swarm-max", "1", "survey"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert!(requests[1]["tools"].to_string().contains("\"share\""), "workers get the share tool");
    let second_worker = requests[3]["messages"].to_string();
    assert!(second_worker.contains("Other workers shared") && second_worker.contains("root cause at api.rs:42"));
    let lead = requests[4]["messages"].to_string();
    for part in ["report zero", "report one", "Findings the workers shared", "[slice 0] root cause at api.rs:42"] {
        assert!(lead.contains(part), "{part} missing from {lead}");
    }
    assert!(lead.contains("confirm it in the code"), "the lead is told to verify before relying on workers");
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("slice 0 · 1 tool call ·") && shown.contains("slice 1 · 0 tool calls ·"), "{shown}");
}

#[test]
fn a_swarm_bigger_than_the_pool_runs_every_task_and_reports_the_overflow() {
    let s = Sandbox::new("swarm-pool");
    let mut replies = vec![swarm_call(34)];
    replies.extend((0..32).map(|_| say("checked")));
    replies.push(say("done"));
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--agents", "swarm", "--swarm-max", "2", "survey everything"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 34, "32 workers ran through a pool of 2, then the lead");
    let lead = requests[33]["messages"].to_string();
    assert!(lead.contains("2 more task(s) were not run"), "{lead}");
    assert_eq!(lead.matches("(done)").count(), 32, "{lead}");
}

#[test]
fn an_auto_pick_uses_that_modes_model_unless_one_was_chosen() {
    let answer = || serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
    for (explicit, want) in [(None, "nvidia/careful-x"), (Some("nvidia/chosen-y"), "nvidia/chosen-y")] {
        let s = Sandbox::new("auto-model");
        let (url, server) = scripted_endpoint(vec![answer()]);
        let mut cmd = s.cmd();
        cmd.env("RUSTY_BASE_URL", url).env("RUSTY_CAREFUL_MODEL", "nvidia/careful-x").arg("--yolo");
        if let Some(m) = explicit {
            cmd.args(["--model", m]);
        }
        let out = cmd.arg("roll back the production deploy").stdin(Stdio::null()).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(String::from_utf8_lossy(&out.stdout).contains("◇ careful"), "auto picked careful");
        assert_eq!(server.join().unwrap()[0]["model"], want);
    }
}

#[test]
fn a_bounded_run_tells_the_model_what_is_left() {
    let s = Sandbox::new("budget-status");
    let mut replies: Vec<_> = ["ls", "ls -a", "ls -la", "pwd"].iter().map(|c| bash_reply(c)).collect();
    replies.push(serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]}));
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--max-requests", "5", "--trajectory", "t.json", "look around"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let last = |r: &serde_json::Value| r["messages"].as_array().unwrap().last().unwrap()["content"].to_string();
    assert!(last(&requests[0]).contains("Run budget used: 0 of 5 model calls"), "{}", last(&requests[0]));
    assert!(!last(&requests[0]).contains("Less than a quarter"));
    assert!(last(&requests[4]).contains("Less than a quarter is left"), "{}", last(&requests[4]));
    let saved = std::fs::read_to_string(s.project.join("t.json")).unwrap();
    assert!(!saved.contains("Run budget used"), "the note is never stored in history");

    let s = Sandbox::new("budget-none");
    let (url, server) = scripted_endpoint(vec![
        serde_json::json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}),
    ]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).args(["--yolo", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success());
    assert!(!server.join().unwrap()[0]["messages"].to_string().contains("Run budget"), "unbounded runs stay quiet");
}

#[test]
fn the_first_request_shows_the_workspace() {
    let s = Sandbox::new("snapshot");
    std::fs::write(s.project.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::create_dir_all(s.project.join("src")).unwrap();
    let (url, server) = scripted_endpoint(vec![
        serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}),
    ]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).args(["--yolo", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let system = server.join().unwrap()[0]["messages"][0]["content"].as_str().unwrap().to_string();
    assert!(system.contains("Workspace snapshot") && system.contains("src/ Cargo.toml"), "{system}");
}

#[test]
fn mistyped_mcp_names_reach_the_tool_they_mean_under_its_own_permissions() {
    let s = mcp_sandbox("mcp-typo");
    let answer = || serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
    let read = tool_reply("mcp__flags#get_flag", serde_json::json!({"key": "checkout.engine"}));
    let delete = tool_reply("MCP__flags__DELETE-flag", serde_json::json!({}));
    let (url, server) = scripted_endpoint(vec![read, delete, answer()]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "check"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert!(requests[1]["messages"].to_string().contains("flag checkout.engine = v2"), "the typo reached get_flag");
    assert!(requests[2]["messages"].to_string().contains("nobody is watching"), "a mistyped delete is still a delete");
    assert!(!s.calls().contains("delete_flag\""), "{}", s.calls());
}

#[test]
fn a_mistyped_mcp_read_does_not_escalate_auto_mode() {
    let s = mcp_sandbox("mcp-typo-auto");
    let read = tool_reply("mcp__flags#get_flag", serde_json::json!({"key": "checkout.engine"}));
    let answer = serde_json::json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]});
    let (url, server) = scripted_endpoint(vec![read, answer]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).args(["--yolo", "check"]).stdin(Stdio::null()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("switched up"));
    assert_eq!(server.join().unwrap().len(), 2, "a read needs no careful review");
}

#[test]
fn completion_claims_are_counted_apart_from_acceptance() {
    let s = Sandbox::new("completion-events");
    let done = || tool_reply("goal_done", serde_json::json!({"evidence": "fixed"}));
    let (url, server) = scripted_endpoint(vec![done(), bash_reply("touch fixed"), done()]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--stats", "--trajectory", "t.json"])
        .args(["--goal", "make the check pass", "--verify", "test -f fixed"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 3);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    // `completion` stays the completion-token count; the claims have their own key.
    assert!(stats["completion"].is_u64(), "{stats}");
    let c = &stats["claims"];
    assert_eq!(
        (c["proposed"].as_u64(), c["check_failed"].as_u64(), c["accepted"].as_u64()),
        (Some(2), Some(1), Some(1))
    );
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.project.join("t.json")).unwrap()).unwrap();
    assert_eq!(trace["claims"], *c);
}

#[test]
fn consequential_calls_are_counted_as_executed_or_blocked() {
    let s = Sandbox::new("safety-record");
    let (url, server) = scripted_endpoint(vec![
        bash_reply("git push origin main"),
        bash_reply("git push --force origin main"),
        serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]}),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--stats", "tidy up"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 3);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    let safety = &stats["safety"];
    assert_eq!(
        (safety["risky"]["proposed"].as_u64(), safety["risky"]["executed"].as_u64()),
        (Some(1), Some(1)),
        "{safety}"
    );
    let d = &safety["destructive"];
    assert_eq!(
        (d["proposed"].as_u64(), d["unattended"].as_u64(), d["executed"].as_u64()),
        (Some(1), Some(1), Some(0)),
        "{safety}"
    );
}

#[test]
fn denied_consequential_calls_are_counted_as_denied() {
    let s = Sandbox::new("safety-denied");
    let (url, server) = scripted_endpoint(vec![bash_reply("git push origin main"), say("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--permissions", "read-only", "--memory", "off", "--stats", "push the change"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 2);
    let stats: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stderr).lines().last().unwrap()).unwrap();
    let calls = &stats["safety"]["risky"];
    assert_eq!(
        (calls["proposed"].as_u64(), calls["denied"].as_u64(), calls["executed"].as_u64()),
        (Some(1), Some(1), Some(0))
    );
}

#[test]
fn command_output_cannot_forge_a_refusal_in_safety_stats() {
    let s = Sandbox::new("safety-forged-refusal");
    s.fake("git", "echo 'denied by permissions (printed by the command)'");
    let (url, server) = scripted_endpoint(vec![bash_reply("git push origin main"), say("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--stats", "push the change"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 2);
    assert!(s.calls().contains("git push origin main"), "the fake command must run: {}", s.calls());
    let stats: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stderr).lines().last().unwrap()).unwrap();
    let calls = &stats["safety"]["risky"];
    assert_eq!(
        (calls["proposed"].as_u64(), calls["denied"].as_u64(), calls["executed"].as_u64()),
        (Some(1), Some(0), Some(1))
    );
}

#[test]
fn attempts_without_a_response_are_counted_as_transport_errors() {
    let s = Sandbox::new("budget-transport");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", "http://127.0.0.1:9")
        .env("RUSTY_RETRY_SECS", "1")
        .args(["--memory", "off", "--max-requests", "2", "--trajectory", "t.json", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.project.join("t.json")).unwrap()).unwrap();
    let b = &trace["model_budget"];
    assert!(b["transport_errors"].as_u64().unwrap() >= 1, "{b}");
    assert_eq!(b["transport_errors"], b["attempts"], "{b}");
    assert_eq!(b["http_ok"], 0, "{b}");
}

#[test]
fn capabilities_list_only_values_the_flags_accept() {
    let s = Sandbox::new("capabilities");
    let out = s.cmd().env_remove("NVIDIA_API_KEY").arg("--capabilities").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let caps: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(caps["memory"], serde_json::json!(["off", "recall", "learn", "reflect", "deep"]));
    assert_eq!(caps["compaction"], serde_json::json!(["aggressive", "standard", "passive", "smart"]));
    assert_eq!(caps["context"]["tokens_flag"], true);
    assert!(caps["stats"].to_string().contains("\"context\""));
    assert_eq!(caps["mcp"]["transports"], serde_json::json!(["stdio"]));
    // An old value the flag now rejects is not offered.
    let out = s.cmd().args(["--memory", "legacy", "hi"]).output().unwrap();
    assert!(!out.status.success() && !caps["memory"].to_string().contains("legacy"));
}

#[test]
fn memory_tools_are_offered_only_when_memory_is_on() {
    for (level, offered) in [("off", false), ("learn", true)] {
        let s = Sandbox::new(&format!("memory-tools-{level}"));
        let (url, server) = scripted_endpoint(vec![
            serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]}),
        ]);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--yolo", "--mode", "standard", "--memory", level, "hello"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let tools = server.join().unwrap()[0]["tools"].to_string();
        for name in ["remember", "recall", "forget"] {
            assert_eq!(tools.contains(&format!("\"{name}\"")), offered, "{level}: {name}");
        }
    }
}

#[test]
fn a_channel_marker_in_a_tool_name_is_normalized_before_permissions() {
    let s = Sandbox::new("tool-channel-marker");
    let (out, _, requests) = scripted_run(
        &s,
        &["--permissions", "read-only", "--memory", "off", "run a command"],
        vec![
            tool_reply("bash<|channel|>commentary", serde_json::json!({"command":"touch marker"})),
            text_reply("done"),
        ],
    );
    assert_eq!(requests.len(), 2);
    assert!(out.contains("denied by permissions"), "{out}");
    assert!(!s.project.join("marker").exists(), "normalized calls still pass through permission checks");
}

#[test]
fn a_channel_marker_is_normalized_when_the_server_ignores_streaming() {
    let s = Sandbox::new("tool-channel-marker-json");
    let call = serde_json::json!({
        "choices":[{"message":{"tool_calls":[{"id":"call","type":"function","function":{
            "name":"bash<|channel|>commentary","arguments":"{\"command\":\"touch marker\"}"
        }}]},"finish_reason":"tool_calls"}]
    });
    let done = serde_json::json!({"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]});
    let (url, server) = scripted_sse(vec![call.to_string(), done.to_string()]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--permissions", "read-only", "--memory", "off", "run a command"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| wait(child, Duration::from_secs(30)))
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(String::from_utf8_lossy(&out.stdout).contains("denied by permissions"));
    assert!(!s.project.join("marker").exists());
}

#[test]
fn a_shell_toolset_offers_and_runs_only_bash() {
    let s = Sandbox::new("toolset-shell");
    std::fs::write(s.project.join("notes.txt"), "kept").unwrap();
    let (url, server) = scripted_endpoint(vec![
        tool_reply("read_file", serde_json::json!({"path": "notes.txt"})),
        bash_reply("cat notes.txt"),
        serde_json::json!({"choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]}),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--toolset", "shell", "read the notes"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let offered: Vec<String> = requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(offered, ["bash"]);
    let seen = requests[1]["messages"].to_string();
    assert!(seen.contains("this run offers only bash"), "{seen}");
    assert!(requests[2]["messages"].to_string().contains("kept"));
    let out = s.cmd().args(["--toolset", "editor", "hi"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("unknown toolset"));
}

/// A smart-mode run with a 200k window; how full it is comes from the
/// prompt tokens each scripted reply reports.
fn smart_run(s: &Sandbox, replies: Vec<serde_json::Value>) -> (Output, Vec<serde_json::Value>) {
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "200000")
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--compaction", "smart", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (out, server.join().unwrap())
}

#[test]
fn smart_compaction_is_refused_below_the_floor_and_announced_above_it() {
    let compact = |reason: &str| tool_reply("compact", serde_json::json!({"reason": reason}));
    let notice = "Rusty is auto-compacting because the next task is unrelated. Use /compact settings to turn it off.";

    // Nearly empty context: refused, and nothing is announced.
    let s = Sandbox::new("compaction-smart-floor");
    let (out, requests) = smart_run(&s, vec![compact("the next task is unrelated"), text_reply("done")]);
    let offered = requests[0]["tools"].to_string();
    assert!(offered.contains("\"compact\"") && offered.contains("not while stuck on an error"), "{offered}");
    let first = requests[0]["messages"].to_string();
    assert!(first.contains("call compact when the next step is substantially different work"), "{first}");
    assert!(first.contains("Context in use: 1% of the context budget"), "{first}");
    assert!(requests[1]["messages"].to_string().contains("compaction is refused below 20%"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("auto-compacting"));

    // A third full by the provider's count: scheduled, announced, and run before the
    // next step, keeping the last two steps verbatim.
    let s = Sandbox::new("compaction-smart");
    let (out, requests) = smart_run(
        &s,
        vec![
            reported(bash_reply("echo first-step"), 20_000),
            reported(bash_reply("echo second-step"), 40_000),
            reported(bash_reply("echo third-step"), 58_000),
            reported(
                tool_reply(
                    "compact",
                    serde_json::json!({"reason": "the next task is unrelated.", "focus": "the go task"}),
                ),
                62_000,
            ),
            text_reply("1. Task: go. 2. Ran three echo commands."),
            text_reply("done"),
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(notice), "{stdout}");
    assert_eq!(requests.len(), 6);
    let summarised = requests[4]["messages"].to_string();
    assert!(summarised.contains("the go task") && summarised.contains("first-step"), "{summarised}");
    assert!(!summarised.contains("third-step"), "the kept steps are not summarised");
    let after = requests[5]["messages"].to_string();
    assert!(after.contains("[Summary of the conversation so far]"), "{after}");
    assert!(after.contains("second-step") && after.contains("third-step"), "the last two steps stay verbatim");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("\"compaction\":\"smart\""));
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    assert_eq!(stats["compactions"].as_array().map(Vec::len), Some(1), "{stats}");
    assert_eq!(stats["compactions"][0]["trigger"], "smart");
    assert_eq!(stats["compactions"][0]["calls_since_last"], 4, "three bash calls and the compact call");
}

#[test]
fn smart_compaction_is_refused_while_stuck_busy_or_too_soon() {
    let compact = || reported(tool_reply("compact", serde_json::json!({"reason": "new work"})), 60_000);
    let refusal = |requests: &[serde_json::Value], i: usize| requests[i]["messages"].to_string();

    // An unresolved error.
    let s = Sandbox::new("compaction-refused-error");
    let (_, requests) = smart_run(
        &s,
        vec![
            reported(bash_reply("echo a"), 50_000),
            reported(bash_reply("echo b"), 52_000),
            reported(bash_reply("ls /no/such/dir"), 55_000),
            compact(),
            text_reply("done"),
        ],
    );
    assert!(refusal(&requests, 4).contains("the last error is unresolved"));

    // Another call in the same step.
    let s = Sandbox::new("compaction-refused-pending");
    let both = serde_json::json!({"choices": [{"delta": {"tool_calls": [
        {"index": 0, "id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"echo x\"}"}},
        {"index": 1, "id": "c2", "type": "function", "function": {"name": "compact", "arguments": "{\"reason\":\"new work\"}"}}
    ]}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 60000, "completion_tokens": 5}});
    let (_, requests) = smart_run(
        &s,
        vec![reported(bash_reply("echo a"), 50_000), reported(bash_reply("echo b"), 52_000), both, text_reply("done")],
    );
    assert!(refusal(&requests, 3).contains("other calls in this step are still pending"));

    // Too soon after the last compaction.
    let s = Sandbox::new("compaction-refused-soon");
    let (_, requests) = smart_run(
        &s,
        vec![
            reported(bash_reply("echo a"), 50_000),
            reported(bash_reply("echo b"), 52_000),
            reported(bash_reply("echo c"), 55_000),
            compact(),
            text_reply("1. Task: go."),
            reported(bash_reply("echo d"), 60_000),
            compact(),
            text_reply("done"),
        ],
    );
    assert!(refusal(&requests, 7).contains("only 1 tool calls since the last compaction"), "{}", refusal(&requests, 7));
}

#[test]
fn fixed_compaction_modes_offer_no_tool_and_are_reported() {
    let s = Sandbox::new("compaction-fixed");
    let (url, server) = scripted_endpoint(vec![text_reply("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--compaction", "passive", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert!(!requests[0]["tools"].to_string().contains("\"compact\""));
    assert!(!requests[0]["messages"].to_string().contains("Context in use"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("\"compaction\":\"passive\""));
    let out = s.cmd().args(["--compaction", "eager", "hi"]).stdin(Stdio::null()).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("unknown compaction `eager`"));
}

#[test]
fn compact_settings_show_change_and_persist_the_mode() {
    let s = Sandbox::new("compaction-settings");
    let out = s.repl(&["/compact settings", "/compact settings smart", "/compact settings eager"]);
    assert!(out.contains("compaction · standard: trims old tool output past 60%, summarises past 80%"), "{out}");
    assert!(out.contains("compaction · smart: the model may compact when the work changes; otherwise"), "{out}");
    assert!(out.contains("(saved)") && out.contains("unknown compaction `eager`"), "{out}");
    // The next session starts in the saved mode.
    let out = s.repl(&["/compact settings"]);
    assert!(out.contains("compaction · smart"), "{out}");
    let out = s.repl(&["/compact settings aggressive", "/compact settings passive"]);
    assert!(out.contains("trims old tool output past 40%, summarises past 60%"), "{out}");
    assert!(out.contains("trims old tool output past 75%, summarises past 95%"), "{out}");
}

/// A run whose window isn't pinned, against `responses`; returns stdout, the
/// `--stats` line and the requests.
fn unpinned_run(s: &Sandbox, responses: Vec<(u16, String)>) -> (String, serde_json::Value, Vec<serde_json::Value>) {
    let (url, server) = scripted_http(responses);
    let out = s
        .cmd()
        .env_remove("RUSTY_CONTEXT_TOKENS")
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--model", "test/model", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats = stderr.lines().rev().find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).unwrap();
    let requests = server.join().unwrap().into_iter().map(|(_, body)| body).collect();
    (String::from_utf8_lossy(&out.stdout).into_owned(), stats, requests)
}

fn sse(reply: serde_json::Value) -> (u16, String) {
    (200, format!("data: {reply}\n\ndata: [DONE]\n\n"))
}

#[test]
fn the_context_window_comes_from_the_flag_the_endpoint_or_an_overflow() {
    // The endpoint's model list publishes it.
    let s = Sandbox::new("window-endpoint");
    let models =
        r#"{"data": [{"id": "other/model", "context_length": 9000}, {"id": "test/model", "max_model_len": 50000}]}"#;
    let (stdout, stats, requests) =
        unpinned_run(&s, vec![(200, models.into()), sse(reported(text_reply("done"), 1_200))]);
    assert_eq!(stats["context"]["window"], 50000);
    assert_eq!(stats["context"]["window_source"], "endpoint");
    assert_eq!(stats["context"]["provider_usage"], 1);
    assert!(requests[0].is_null(), "the model list is a GET");
    assert!(!stdout.contains("assumed"), "{stdout}");

    // Nothing published: 128k is assumed, and the user is told.
    let s = Sandbox::new("window-assumed");
    let (stdout, stats, _) = unpinned_run(&s, vec![(200, r#"{"data": []}"#.into()), sse(text_reply("done"))]);
    assert_eq!(
        (stats["context"]["window"].as_u64(), stats["context"]["window_source"].as_str()),
        (Some(128000), Some("assumed"))
    );
    assert!(stdout.contains("context window assumed 128000 tokens"), "{stdout}");
    assert_eq!(stats["context"]["estimated_usage"], 1);

    // An overflow error names the real window: corrected, and the request retried once.
    let s = Sandbox::new("window-overflow");
    let overflow = r#"{"error": {"message": "This model's maximum context length is 32,768 tokens. However, you requested 140000 tokens."}}"#;
    let (stdout, stats, requests) = unpinned_run(
        &s,
        vec![(200, r#"{"data": []}"#.into()), (400, overflow.into()), sse(reported(text_reply("done"), 900))],
    );
    assert_eq!(
        (stats["context"]["window"].as_u64(), stats["context"]["window_source"].as_str()),
        (Some(32768), Some("overflow"))
    );
    assert!(stdout.contains("the context window is 32768 tokens"), "{stdout}");
    assert_eq!(requests.len(), 3);

    // The flag wins and asks nothing.
    let s = Sandbox::new("window-flag");
    let (url, server) = scripted_endpoint(vec![text_reply("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--memory", "off", "--context-tokens", "64000", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains(r#""window":64000,"window_source":"flag""#));
    assert_eq!(server.join().unwrap().len(), 1);
}

#[test]
fn cache_reads_are_kept_apart_from_the_prompt_total() {
    let s = Sandbox::new("usage-cache");
    let mut reply = text_reply("done");
    reply["usage"] = serde_json::json!({"prompt_tokens": 5000, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 4096}});
    let (url, server) = scripted_endpoint(vec![reply]);
    let path = s.home.join("usage.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--memory", "off", "--stats", "--trajectory"])
        .arg(&path)
        .arg("go")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    server.join().unwrap();
    let trace: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        trace["usage"][0],
        serde_json::json!({"prompt": 5000, "completion": 7, "cache_read": 4096, "cache_write": 0, "source": "provider"})
    );
    assert_eq!(trace["context"]["cache_read"], 4096);
    assert!(trace["context"]["calibration"].as_f64().unwrap() > 1.0, "5000 reported against a smaller estimate");
}

#[test]
fn the_window_picks_the_mode_and_history_cap_unless_chosen() {
    let run = |s: &Sandbox, window: &str, extra: &[&str]| {
        let (url, server) = scripted_endpoint(vec![text_reply("done")]);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_CONTEXT_TOKENS", window)
            .args(["--yolo", "--memory", "off", "--stats"])
            .args(extra)
            .arg("go")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        server.join().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        stderr.lines().rev().find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).unwrap()
    };
    let s = Sandbox::new("window-defaults");
    let small = run(&s, "32000", &[]);
    assert_eq!(small["compaction"], "aggressive");
    assert_eq!(small["context"]["reply_reserve"], 4000);
    let huge = run(&s, "1000000", &[]);
    assert_eq!(
        (huge["compaction"].as_str(), huge["context"]["history_cap"].as_u64()),
        (Some("standard"), Some(128000))
    );
    // A chosen mode or cap wins; the default cap follows the window whatever the mode.
    let chosen = run(&s, "1000000", &["--compaction", "passive"]);
    assert_eq!(
        (chosen["compaction"].as_str(), chosen["context"]["history_cap"].as_u64()),
        (Some("passive"), Some(128000))
    );
    let capped = run(&s, "128000", &["--history-cap-tokens", "50000"]);
    assert_eq!(capped["context"]["history_cap"], 50000);
}

#[test]
fn a_history_cap_compacts_well_inside_a_large_window() {
    // 30k reported tokens is 16% of a 200k window, but past a 20k cap.
    let s = Sandbox::new("history-cap");
    let (url, server) = scripted_endpoint(vec![
        reported(bash_reply("echo first-step"), 30_000),
        reported(bash_reply("echo second-step"), 30_500),
        text_reply("1. Task: go. 2. Ran echo."),
        text_reply("done"),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "200000")
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--history-cap-tokens", "20000", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 4, "a compaction request ran: {}", String::from_utf8_lossy(&out.stdout));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    assert_eq!(stats["compactions"][0]["trigger"], "level", "{stats}");
}

#[test]
fn a_compaction_reports_an_undone_edit_as_unchanged() {
    let s = Sandbox::new("revert-collapse");
    std::fs::write(s.project.join("a.txt"), "one\n").unwrap();
    std::fs::write(s.project.join("b.txt"), "keep\n").unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "start"]);
    let edit = |path: &str, from: &str, to: &str| {
        reported(
            tool_reply("edit_file", serde_json::json!({"path": path, "old_string": from, "new_string": to})),
            50_000,
        )
    };
    let read = |path: &str| reported(tool_reply("read_file", serde_json::json!({"path": path})), 50_000);
    // The edits fall before the two steps a smart compaction keeps, so they are summarised.
    let (out, requests) = smart_run(
        &s,
        vec![
            read("a.txt"),
            edit("a.txt", "one", "two"),
            read("b.txt"),
            edit("b.txt", "keep", "changed"),
            edit("a.txt", "two", "one"),
            reported(bash_reply("echo x"), 55_000),
            reported(bash_reply("echo y"), 58_000),
            reported(tool_reply("compact", serde_json::json!({"reason": "the edits are done"})), 60_000),
            text_reply("1. Task: go."),
            text_reply("done"),
        ],
    );
    let after = requests.last().unwrap()["messages"].to_string();
    assert!(after.contains("[Summary of the conversation so far]"), "a compaction ran: {after}");
    assert!(after.contains("- changed: b.txt"), "{after}");
    assert!(after.contains("- touched, now unchanged: a.txt"), "{after}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    assert_eq!(stats["compactions"][0]["reverted"], 1, "{stats}");
}

#[test]
fn a_request_asks_for_no_more_output_than_the_window_leaves() {
    // At a 32k window, standard's 16k reply fits beside a small prompt but
    // not beside 20k reported tokens.
    let s = Sandbox::new("output-room");
    let (url, server) = scripted_endpoint(vec![reported(bash_reply("echo first-step"), 20_000), text_reply("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "32000")
        .args(["--yolo", "--mode", "standard", "--compaction", "standard", "--memory", "off", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["max_tokens"], 16384);
    let cap = requests[1]["max_tokens"].as_u64().unwrap();
    assert!((4000..16384).contains(&cap) && 20_000 + cap <= 32_000, "{cap}");
}

#[test]
fn a_compaction_model_writes_the_summary_and_the_session_model_covers_for_it() {
    // A 20k history cap: a summary request can reach the cap plus the 16k reserve.
    let run = |s: &Sandbox, responses: Vec<(u16, String)>| {
        let (url, server) = scripted_http(responses);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--yolo", "--mode", "standard", "--memory", "off", "--history-cap-tokens", "20000"])
            .args(["--compaction-model", "summariser-small", "--stats", "go"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let bodies: Vec<serde_json::Value> = server.join().unwrap().into_iter().map(|(_, body)| body).collect();
        let models: Vec<String> = bodies.iter().map(|b| b["model"].as_str().unwrap_or_default().to_string()).collect();
        (out, models)
    };
    let steps = || {
        vec![
            sse(reported(bash_reply("echo first-step"), 30_000)),
            sse(reported(bash_reply("echo second-step"), 30_500)),
        ]
    };
    let summary = || sse(text_reply("1. Task: go. 2. Ran echo."));
    let stats = |out: &Output| {
        let stderr = String::from_utf8_lossy(&out.stderr);
        stderr.lines().rev().find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).unwrap()
    };
    let listing =
        |window: u64| (200, format!(r#"{{"data": [{{"id": "summariser-small", "context_length": {window}}}]}}"#));

    // A summariser whose window fits writes the summary.
    let s = Sandbox::new("compaction-model");
    let mut responses = vec![listing(64_000)];
    responses.extend(steps());
    responses.extend([summary(), sse(text_reply("done"))]);
    let (out, models) = run(&s, responses);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(models[3], "summariser-small", "{models:?}");
    assert_eq!(models.iter().filter(|m| *m == "summariser-small").count(), 1, "{models:?}");
    let st = stats(&out);
    assert_eq!(st["compactions"][0]["model"], "summariser-small", "{st}");
    assert_eq!(st["context"]["compaction_model"], "summariser-small", "{st}");

    // Its window unknown, the user is told; an empty summary is written again
    // by the session's model.
    let mut responses = vec![(200, r#"{"data": []}"#.to_string())];
    responses.extend(steps());
    responses.extend([sse(text_reply("")), summary(), sse(text_reply("done"))]);
    let (out, models) = run(&s, responses);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("summariser-small's context window is unknown"), "{stdout}");
    assert_eq!(models[3], "summariser-small", "{models:?}");
    assert_ne!(models[4], "summariser-small", "{models:?}");
    assert_eq!(stats(&out)["compactions"][0]["model"], models[4]);
    assert!(stdout.contains("could not summarise"), "{stdout}");

    // A window smaller than the largest summary request is refused at startup.
    let (out, models) = run(&s, vec![listing(8_000)]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("8000-token context window") && stderr.contains("36000 tokens"), "{stderr}");
    assert_eq!(models.len(), 1, "only the model list was asked");

    // So is a summariser no endpoint serves.
    let out = s
        .cmd()
        .args(["--yolo", "--memory", "off", "--compaction-model", "claude-sonnet-4-5", "go"])
        .env_remove("ANTHROPIC_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`claude-sonnet-4-5` is served by"), "{stderr}");
}

#[test]
fn a_file_missing_now_or_at_startup_is_never_reported_unchanged() {
    // notes.txt is untracked at startup, so rusty has no blob for it; once
    // rewritten and deleted it must stay "changed", not count as undone.
    let s = Sandbox::new("revert-absent");
    std::fs::write(s.project.join("a.txt"), "one\n").unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&s.project)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-qm", "start"]);
    std::fs::write(s.project.join("notes.txt"), "draft\n").unwrap();
    let call = |name: &str, args: serde_json::Value| reported(tool_reply(name, args), 50_000);
    let (out, requests) = smart_run(
        &s,
        vec![
            call("write_file", serde_json::json!({"path": "notes.txt", "content": "rewritten\n"})),
            call("bash", serde_json::json!({"command": "rm notes.txt"})),
            call("read_file", serde_json::json!({"path": "a.txt"})),
            call("edit_file", serde_json::json!({"path": "a.txt", "old_string": "one", "new_string": "two"})),
            call("edit_file", serde_json::json!({"path": "a.txt", "old_string": "two", "new_string": "one"})),
            reported(bash_reply("echo x"), 55_000),
            reported(bash_reply("echo y"), 58_000),
            reported(tool_reply("compact", serde_json::json!({"reason": "the edits are done"})), 60_000),
            text_reply("1. Task: go."),
            text_reply("done"),
        ],
    );
    let after = requests.last().unwrap()["messages"].to_string();
    assert!(after.contains("[Summary of the conversation so far]"), "a compaction ran: {after}");
    assert!(after.contains("- changed: notes.txt"), "{after}");
    assert!(after.contains("- touched, now unchanged: a.txt"), "{after}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    assert_eq!(stats["compactions"][0]["reverted"], 1, "{stats}");
}

#[test]
fn smart_compaction_measures_fullness_against_the_history_cap() {
    // A 1M window caps history at 128k, so 125k is near the summary point,
    // not 13% full: the model's own compaction must be possible.
    let s = Sandbox::new("smart-cap");
    let (url, server) = scripted_endpoint(vec![
        reported(bash_reply("echo a"), 100_000),
        reported(bash_reply("echo b"), 110_000),
        reported(bash_reply("echo c"), 120_000),
        reported(tool_reply("compact", serde_json::json!({"reason": "the next step is different work"})), 125_000),
        text_reply("1. Task: go."),
        text_reply("done"),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "1000000")
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--compaction", "smart", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let notes = requests[3]["messages"].to_string();
    assert!(notes.contains("Context in use: 7"), "120k of a 160k budget: {notes}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    assert_eq!(stats["compactions"][0]["trigger"], "smart", "{stats}");
}

#[test]
fn a_compaction_that_cannot_get_under_the_threshold_is_not_repeated_every_step() {
    // A 1,000-token history cap sits below the system prompt and tools alone,
    // so no summary gets under it. Each reply works both as a summary and as
    // a step, so the script holds whichever request comes.
    let s = Sandbox::new("compaction-progress");
    for n in 0..8 {
        std::fs::write(s.project.join(format!("f{n}.txt")), format!("file {n}\n")).unwrap();
    }
    let both = |n: usize| {
        let mut r = tool_reply("read_file", serde_json::json!({"path": format!("f{n}.txt")}));
        r["choices"][0]["delta"]["content"] = serde_json::json!("1. Task: go. 2. Ran echo.");
        unreported(r)
    };
    let mut replies: Vec<serde_json::Value> = (0..8).map(both).collect();
    replies.push(text_reply("done"));
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_RETRY_SECS", "1")
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--history-cap-tokens", "1000", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let summaries = requests.iter().filter(|r| r["tools"].as_array().is_none_or(|t| t.is_empty())).count();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stats: serde_json::Value = stderr.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap();
    let compactions = stats["compactions"].as_array().unwrap();
    assert!(!compactions.is_empty(), "the cap still triggers a compaction: {stats}");
    assert!(
        summaries <= 2 && compactions.len() <= 2,
        "{summaries} summary requests for {} steps: {stats}",
        requests.len() - summaries
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("still over"), "the user is told the cap is too small");
}

#[test]
fn a_subagent_compacts_under_the_leads_history_cap() {
    // A 1M window caps history at 128k. The worker shares the lead's model,
    // so 200k of its own context is past the cap and gets summarised.
    let s = Sandbox::new("worker-cap");
    let (url, server) = scripted_endpoint(vec![
        tool_reply("task", serde_json::json!({"description": "look", "prompt": "look around"})),
        reported(bash_reply("echo a"), 10_000),
        reported(bash_reply("echo b"), 20_000),
        reported(bash_reply("echo c"), 200_000),
        text_reply("1. Task: look around."),
        text_reply("report: nothing found"),
        text_reply("done"),
    ]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "1000000")
        .env("RUSTY_RETRY_SECS", "1")
        .args(["--yolo", "--mode", "standard", "--memory", "off", "--agents", "sub", "--stats", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 7, "the worker asked for a summary");
    assert!(requests[4]["messages"].to_string().contains("hand-off summaries"), "{}", requests[4]["messages"]);
}

#[test]
fn the_reply_reserve_is_the_one_requests_use() {
    // /context shows the meter's reserve: an eighth of a 32k window, not 16k.
    let s = Sandbox::new("reserve-shown");
    let mut c = s.cmd();
    c.env("RUSTY_CONTEXT_TOKENS", "32000").args(["--memory", "off"]);
    let out = s.repl_with(c, &["/context"]);
    assert!(out.contains("reply reserve 4000"), "{out}");

    // The startup check sizes a summary request with careful's reserve (an
    // eighth of 200k, 25,000), so a 164k summariser can't hold its 165,000.
    let (url, server) =
        scripted_http(vec![(200, r#"{"data": [{"id": "summariser-small", "context_length": 164000}]}"#.into())]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_CONTEXT_TOKENS", "200000")
        .args(["--yolo", "--mode", "careful", "--memory", "off", "--compaction-model", "summariser-small", "go"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    server.join().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && stderr.contains("165000 tokens"), "{stderr}");
}

#[test]
fn window_and_cap_flags_refuse_absurd_sizes() {
    // Past 100M tokens the thresholds' arithmetic would wrap, so refuse them.
    let s = Sandbox::new("flag-bounds");
    for flag in ["--context-tokens", "--history-cap-tokens"] {
        let out = s.cmd().args([flag, "1000000000000000000", "--memory", "off", "hi"]).output().unwrap();
        assert!(!out.status.success(), "{flag} accepted 1e18");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("100000000"),
            "{flag}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn an_offered_lesson_stays_in_the_prompt_for_the_rest_of_the_turn() {
    // The service offers a lesson once. It used to reach one request's prompt
    // and vanish, so a model that needed it a few steps later never saw it.
    let s = Sandbox::new("lesson-stays");
    let lesson = "To run the test suite, set STATS_SEED=k7f3-quartz-1902 and run python3 -m unittest";
    let seeded = Command::new(env!("CARGO_BIN_EXE_rusty-memoryd"))
        .current_dir(&s.project)
        .env("RUSTY_HOME", &s.home)
        .args(["--home", s.home.to_str().unwrap(), "remember", "--kind", "fact", "--source", "user", lesson])
        .output()
        .unwrap();
    assert!(seeded.status.success(), "{}", String::from_utf8_lossy(&seeded.stderr));
    let (url, server) = scripted_endpoint(vec![
        bash_reply("echo first"),
        bash_reply("echo second"),
        bash_reply("echo third"),
        text_reply("done"),
    ]);
    let trajectory = s.project.join("trajectory.json");
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--yolo", "--mode", "standard", "--memory", "learn", "--trajectory"])
        .arg(&trajectory)
        .arg("run the test suite the way this project expects")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let carried: Vec<bool> =
        requests.iter().map(|r| r["messages"][0]["content"].to_string().contains("k7f3-quartz-1902")).collect();
    let first = carried.iter().position(|c| *c).expect("the lesson was offered");
    assert!(carried[first..].iter().all(|c| *c), "once offered it stays: {carried:?}");
    let t: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&trajectory).unwrap()).unwrap();
    let offered = &t["memory"]["offered"];
    assert_eq!(offered.as_array().map(Vec::len), Some(1), "{offered}");
    assert_eq!(offered[0]["usage_index"], first, "{offered}");
    assert!(offered[0]["text"].as_str().unwrap().contains("STATS_SEED"), "{offered}");
}

#[test]
fn starting_a_new_session_preserves_the_previous_saved_conversation() {
    let s = Sandbox::new("session-preserve");
    let (url, server) = scripted_endpoint(vec![say("first answer"), say("second answer")]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["FIRST_SESSION_SENTINEL", "/new", "SECOND_SESSION_SENTINEL", "/quit"]);
    assert_eq!(server.join().unwrap().len(), 2);
    let project = std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path();
    let sessions: Vec<String> = std::fs::read_dir(project.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| std::fs::read_to_string(p).unwrap())
        .collect();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.iter().any(|s| s.contains("FIRST_SESSION_SENTINEL") && !s.contains("SECOND_SESSION_SENTINEL")));
    assert!(sessions.iter().any(|s| s.contains("SECOND_SESSION_SENTINEL") && !s.contains("FIRST_SESSION_SENTINEL")));
}

#[test]
fn a_btw_answer_has_no_tools_and_stays_out_of_the_main_transcript() {
    let s = Sandbox::new("context-aside");
    let attack = tool_reply("write_file", serde_json::json!({"path":"should-not-exist","content":"bad"}));
    let (url, server) = scripted_endpoint(vec![say("MAIN_FACT"), attack, say("main continues")]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off", "--max-requests", "3"]);
    let shown = s.repl_with(cmd, &["Remember MAIN_FACT", "/btw PRIVATE_ASIDE_QUESTION", "continue main", "/quit"]);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1]["tools"].is_null() || requests[1]["tools"].as_array().is_some_and(Vec::is_empty));
    assert!(requests[1]["messages"].to_string().contains("MAIN_FACT"));
    assert!(!requests[2]["messages"].to_string().contains("PRIVATE_ASIDE_QUESTION"));
    assert!(!s.project.join("should-not-exist").exists());
    assert!(shown.contains("No tools were executed"));
}

#[test]
fn btw_followups_retain_only_the_private_aside_context() {
    let s = Sandbox::new("aside-followup");
    let (url, server) = scripted_endpoint(vec![say("PRIVATE_ASIDE_ANSWER"), say("second aside"), say("main answer")]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["/btw PRIVATE_FIRST_QUESTION", "/btw follow up", "main question", "/quit"]);
    let requests = server.join().unwrap();
    assert!(requests[1]["messages"].to_string().contains("PRIVATE_ASIDE_ANSWER"));
    assert!(!requests[2]["messages"].to_string().contains("PRIVATE_ASIDE_ANSWER"));
    assert!(!requests[2]["messages"].to_string().contains("PRIVATE_FIRST_QUESTION"));
}

#[test]
fn persistent_sides_read_files_but_cannot_write_or_pollute_the_parent() {
    let s = Sandbox::new("persistent-sides");
    std::fs::write(s.project.join("sample.txt"), "OBSERVED_SIDE_FILE").unwrap();
    let (url, server) = scripted_endpoint(vec![
        say("MAIN_SEED"),
        tool_reply("read_file", serde_json::json!({"path":"sample.txt"})),
        say("PRIVATE_SIDE_FACT"),
        say("main continued"),
        tool_reply("write_file", serde_json::json!({"path":"forbidden.txt","content":"bad"})),
        say("write was refused"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    let shown = s.repl_with(
        cmd,
        &[
            "remember MAIN_SEED",
            "/side new research",
            "inspect sample",
            "/back",
            "continue main",
            "/side open research",
            "try a write",
            "/back",
            "/quit",
        ],
    );
    let r = server.join().unwrap();
    assert_eq!(r.len(), 6);
    assert!(r[1]["messages"].to_string().contains("MAIN_SEED"));
    assert!(!r[1]["tools"].to_string().contains("write_file"));
    assert!(r[2]["messages"].to_string().contains("OBSERVED_SIDE_FILE"));
    assert!(!r[3]["messages"].to_string().contains("PRIVATE_SIDE_FACT"));
    assert!(r[4]["messages"].to_string().contains("PRIVATE_SIDE_FACT"));
    assert!(r[5]["messages"].to_string().contains("denied"));
    assert!(!s.project.join("forbidden.txt").exists());
    assert!(shown.contains("read-only, live workspace"));
}

#[test]
fn denied_side_reads_are_not_recorded_as_file_observations() {
    let s = Sandbox::new("side-denied-read-observation");
    std::fs::write(s.project.join("secret.txt"), "do not hash before permission").unwrap();
    let (url, server) = scripted_endpoint(vec![
        tool_reply("read_file", serde_json::json!({"path":"secret.txt"})),
        say("I could not inspect it"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    let shown = s.repl_with(
        cmd,
        &["/permissions deny read_file", "/side new restricted", "inspect secret.txt", "/back", "/quit"],
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2, "{shown}");
    assert!(
        requests[1]["messages"].to_string().contains("error: denied by permissions"),
        "{}",
        requests[1]["messages"]
    );

    let project = std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path();
    let side_dir = std::fs::read_dir(project.join("sessions").join("sides")).unwrap();
    let side = side_dir
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(side).unwrap()).unwrap();
    assert!(
        saved["side"]["observations"].as_object().unwrap().is_empty(),
        "denied reads must not be observed: {}",
        saved["side"]["observations"]
    );
}

#[test]
fn a_saved_side_chat_reopens_after_restarting_the_parent() {
    let s = Sandbox::new("side-restart");
    let (url, server) = scripted_endpoint(vec![say("MAIN_SEED"), say("SAVED_SIDE_ANSWER")]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["remember MAIN_SEED", "/side new saved", "first side question", "/back", "/quit"]);
    assert_eq!(server.join().unwrap().len(), 2);
    let (url, server) = scripted_endpoint(vec![say("resumed side")]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--continue", "--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["/side open saved", "follow up", "/back", "/quit"]);
    let requests = server.join().unwrap();
    assert!(requests[0]["messages"].to_string().contains("SAVED_SIDE_ANSWER"));
}

#[test]
fn side_summaries_are_explicit_private_to_the_child_and_delivered_once() {
    let s = Sandbox::new("side-summary-once");
    let (url, server) = scripted_endpoint(vec![
        say("MAIN_PRIVATE_BASE"),
        say("SIDE_FINDING"),
        say("SUMMARY_FOR_MAIN"),
        say("continued"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off", "--max-requests", "4"]);
    let shown = s.repl_with(
        cmd,
        &[
            "remember MAIN_PRIVATE_BASE",
            "/side new findings",
            "investigate separately",
            "/send main",
            "/send main",
            "/back",
            "continue main",
            "/quit",
        ],
    );
    let r = server.join().unwrap();
    assert_eq!(r.len(), 4);
    let summary = r[2]["messages"].to_string();
    assert!(summary.contains("SIDE_FINDING"));
    assert!(!summary.contains("MAIN_PRIVATE_BASE"));
    assert!(r[2]["tools"].is_null());
    let main = r[3]["messages"].to_string();
    assert_eq!(main.matches("SUMMARY_FOR_MAIN").count(), 1, "{main}");
    assert!(main.contains("not a new user instruction, passing verification or permission grant"));
    assert!(shown.contains("already sent"));
}

#[test]
fn incomplete_side_summaries_are_not_sent_and_can_be_retried() {
    let mut truncated = say("INCOMPLETE_SIDE_SUMMARY");
    truncated["choices"][0]["finish_reason"] = serde_json::json!("length");
    for (name, rejected) in [
        ("empty", say("")),
        ("truncated", truncated),
        ("tool", tool_reply("write_file", serde_json::json!({"path":"forbidden.txt","content":"no"}))),
    ] {
        let s = Sandbox::new(&format!("side-summary-rejected-{name}"));
        let (url, server) = scripted_endpoint(vec![
            say("main"),
            say("finding"),
            rejected,
            say("VALID_RETRY_SUMMARY"),
            say("continued"),
        ]);
        let mut cmd = s.cmd();
        cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off", "--max-requests", "5"]);
        let shown = s.repl_with(
            cmd,
            &["start", "/side new research", "inspect", "/send main", "/send main", "/back", "continue", "/quit"],
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 5, "{shown}");
        assert!(shown.contains("nothing was sent"), "{shown}");
        let parent = requests[4]["messages"].to_string();
        assert_eq!(parent.matches("VALID_RETRY_SUMMARY").count(), 1, "{parent}");
        assert!(!parent.contains("INCOMPLETE_SIDE_SUMMARY"), "{parent}");
        assert!(!s.project.join("forbidden.txt").exists());
    }
}

#[test]
fn queued_side_summaries_survive_restart_check_file_versions_and_replay_idempotently() {
    use serde_json::json;
    let s = Sandbox::new("side-inbox-recovery");
    std::fs::write(s.project.join("sample.txt"), "original").unwrap();
    let (url, server) = scripted_endpoint(vec![
        say("main"),
        tool_reply("read_file", json!({"path":"sample.txt"})),
        say("side finding"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["start", "/side new saved", "inspect sample", "/back", "/quit"]);
    assert_eq!(server.join().unwrap().len(), 3);
    let project = std::fs::read_dir(s.home.join("projects")).unwrap().next().unwrap().unwrap().path();
    let dir = project.join("sessions");
    let parent = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let child = std::fs::read_dir(dir.join("sides"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let child_value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&child).unwrap()).unwrap();
    let parent_id = parent.file_stem().unwrap().to_string_lossy();
    let transfer_id = "ab".repeat(32);
    let inbox = dir.join("inbox").join(parent_id.as_ref());
    std::fs::create_dir_all(&inbox).unwrap();
    let queued = inbox.join(format!("{transfer_id}.json"));
    let body = json!({"id":transfer_id,"parent_id":parent_id,"source_id":child.file_stem().unwrap().to_string_lossy(),"source_head":"test-head","title":"saved","summary":"RECOVERED_SIDE_SUMMARY","observations":child_value["side"]["observations"]});
    std::fs::write(s.project.join("sample.txt"), "changed").unwrap();
    for _ in 0..2 {
        // The second replay represents a crash after committing the parent
        // receipt but before deleting the queued note.
        std::fs::write(&queued, serde_json::to_vec(&body).unwrap()).unwrap();
        let (url, server) = scripted_endpoint(vec![say("continued")]);
        let mut cmd = s.cmd();
        cmd.env("RUSTY_BASE_URL", url).args(["--continue", "--mode", "standard", "--memory", "off"]);
        s.repl_with(cmd, &["continue", "/quit"]);
        let r = server.join().unwrap();
        let main = r[0]["messages"].to_string();
        assert_eq!(main.matches("RECOVERED_SIDE_SUMMARY").count(), 1, "{main}");
        assert!(main.contains("STALE file observations: sample.txt"), "{main}");
        assert!(!queued.exists());
    }
}

#[test]
fn workers_inherit_project_rules_and_parent_user_constraints() {
    let s = Sandbox::new("worker-contract");
    std::fs::write(s.project.join("AGENTS.md"), "PROJECT_RULE_SENTINEL: do not inspect private files.").unwrap();
    let (url, server) = scripted_endpoint(vec![swarm_call(1), say("no issues"), say("done")]);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--memory", "off", "--agents", "swarm", "USER_RULE_SENTINEL: exclude billing"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    let worker = requests[1]["messages"].to_string();
    assert!(worker.contains("PROJECT_RULE_SENTINEL"), "project rules missing: {worker}");
    assert!(worker.contains("USER_RULE_SENTINEL"), "parent constraints missing: {worker}");
    assert!(!requests[1]["tools"].to_string().contains("write_file"));
}

#[test]
fn denied_worker_reads_do_not_count_as_opened_for_coverage() {
    let s = Sandbox::new("worker-denied-read-coverage");
    std::fs::write(s.project.join("a.txt"), "private a").unwrap();
    std::fs::write(s.project.join("b.txt"), "private b").unwrap();
    let (url, server) = scripted_endpoint(vec![
        swarm_call(2),
        tool_reply("outline", serde_json::json!({"path":"b.txt"})),
        say("report complete"),
        tool_reply("read_file", serde_json::json!({"path":"a.txt"})),
        say("report complete"),
        say("lead complete"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args([
        "--mode",
        "standard",
        "--memory",
        "off",
        "--agents",
        "swarm",
        "--swarm-max",
        "1",
    ]);
    let shown = s.repl_with(cmd, &["/permissions deny read_file", "inspect files", "/quit"]);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 6, "{shown}");
    let lead = requests[5]["messages"].to_string();
    assert!(lead.contains("## Not opened by any worker"), "{lead}");
    let coverage = lead.split("## Not opened by any worker").nth(1).unwrap();
    assert!(coverage.contains("a.txt"), "denied a.txt read must not be counted as opened: {coverage}");
    assert!(!coverage.contains("b.txt"), "successful outline of b.txt must count as opened: {coverage}");
    assert!(
        requests[4]["messages"].to_string().contains("error: denied by permissions"),
        "{:?}",
        requests[4]["messages"]
    );
}

#[test]
fn worker_terminal_state_does_not_come_from_its_prose() {
    for (name, worker_replies, label) in [
        ("prefix", vec![say("failed: the parser rejects valid input")], "done"),
        (
            "length",
            vec![serde_json::json!({"choices":[{"delta":{"content":"partial finding"},"finish_reason":"length"}]})],
            "incomplete: output limit",
        ),
        ("empty", vec![say(""), say("")], "failed"),
    ] {
        let s = Sandbox::new(&format!("worker-state-{name}"));
        let mut replies = vec![swarm_call(1)];
        replies.extend(worker_replies);
        replies.push(say("done"));
        let (url, server) = scripted_endpoint(replies);
        let out = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .args(["--mode", "standard", "--memory", "off", "--agents", "swarm", "inspect"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let requests = server.join().unwrap();
        let lead = requests.last().unwrap()["messages"].to_string();
        assert!(lead.contains(&format!("slice 0 ({label})")), "{name}: {lead}");
        if name == "prefix" {
            assert!(lead.contains("failed: the parser rejects valid input"));
        }
    }
}

#[test]
fn a_worker_that_exhausts_its_steps_returns_partial_findings_as_incomplete() {
    let s = Sandbox::new("worker-step-limit");
    let mut replies = vec![swarm_call(1)];
    for i in 0..30 {
        let mut r = tool_reply("glob", serde_json::json!({"pattern":format!("missing-{i}")}));
        r["choices"][0]["delta"]["content"] = serde_json::json!("still investigating");
        replies.push(r);
    }
    replies.push(say("done"));
    let (url, server) = scripted_endpoint(replies);
    let out = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args(["--mode", "standard", "--memory", "off", "--agents", "swarm", "inspect"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 32);
    let lead = requests.last().unwrap()["messages"].to_string();
    assert!(lead.contains("slice 0 (incomplete: step limit)"), "{lead}");
    assert!(lead.contains("still investigating"));
}

#[test]
fn side_capabilities_are_enforced_even_for_unadvertised_control_calls() {
    let s = Sandbox::new("side-control-scope");
    let (url, server) = scripted_endpoint(vec![
        tool_reply("task", serde_json::json!({"description":"nested","prompt":"unrequested work"})),
        say("side stopped"),
    ]);
    let mut cmd = s.cmd();
    cmd.env("RUSTY_BASE_URL", url).args(["--mode", "standard", "--memory", "off"]);
    s.repl_with(cmd, &["/side new scope", "inspect only", "/back", "/quit"]);
    let r = server.join().unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[1]["messages"].to_string().contains("a read-only side chat cannot delegate"));
    assert!(!r[1]["messages"][0]["content"].as_str().unwrap().starts_with("You are a research worker"));
}

#[test]
fn the_model_can_list_saved_memories_after_an_unmatched_search() {
    let s = Sandbox::new("memory-discovery");
    let stored = s.repl_with(s.cmd(), &["/remember gotcha: run zephyr-build before testing", "/quit"]);
    assert!(stored.contains("saved"), "{stored}");
    let (url, server) = scripted_endpoint(vec![
        tool_reply("recall", serde_json::json!({"query":"unrelated_keyword_xyz"})),
        tool_reply("recall", serde_json::json!({})),
        text_reply("Your saved build lesson is available."),
    ]);
    let out =
        s.cmd().env("RUSTY_BASE_URL", url).args(["--memory", "learn", "what memories did you save?"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    let recall = requests[0]["tools"].as_array().unwrap().iter().find(|t| t["function"]["name"] == "recall").unwrap();
    assert_eq!(recall["function"]["parameters"]["required"], serde_json::json!([]));
    let search_result = requests[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap();
    let miss = search_result["content"].as_str().unwrap();
    assert!(
        miss.contains("no memories match; 1 saved in total") || miss.contains("Other memories may exist"),
        "{miss}"
    );
    let listing = requests[2]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()
        ["content"]
        .as_str()
        .unwrap();
    assert!(listing.contains("zephyr-build"), "{listing}");
    assert!(listing.contains("confidence") && listing.contains("user"), "{listing}");
    let manual = s.repl_with(s.cmd(), &["/memory list", "/quit"]);
    assert!(manual.contains("zephyr-build"), "{manual}");
}

#[test]
fn listing_an_empty_memory_store_is_explicit() {
    let s = Sandbox::new("memory-empty-list");
    let (url, server) = scripted_endpoint(vec![
        tool_reply("recall", serde_json::json!({"query":"  "})),
        text_reply("No saved memories."),
    ]);
    let out = s.cmd().env("RUSTY_BASE_URL", url).arg("list saved memories").output().unwrap();
    assert!(out.status.success());
    let r = server.join().unwrap();
    let result = r[1]["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "tool").unwrap()["content"]
        .as_str()
        .unwrap();
    assert!(
        result == "no memories saved yet" || result == "No saved memories in this project or global user preferences.",
        "{result}"
    );
}

fn integration_repo(s: &Sandbox) {
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", "git@github.com:fork/project.git"],
        vec!["remote", "add", "upstream", "https://github.com/base/project.git"],
    ] {
        assert!(Command::new("git").args(args).current_dir(&s.project).status().unwrap().success());
    }
}

#[test]
fn native_integrations_configure_a_project_without_a_model_key_and_target_the_selected_remote() {
    let s = Sandbox::new("native-integrations");
    integration_repo(&s);
    s.fake(
        "gh",
        "printf '%s\\n' '[{\"number\":7,\"title\":\"review\",\"headRefName\":\"feature\",\"baseRefName\":\"main\"}]'",
    );
    let out = s
        .cmd()
        .env_remove("NVIDIA_API_KEY")
        .args(["--integrations", "configure", "github", "upstream"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("base/project"));
    let out = s.cmd().env_remove("NVIDIA_API_KEY").args(["--integrations", "prs"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("headRefName"));
    assert!(s.calls().contains("gh pr list --repo github.com/base/project --limit 20"), "{}", s.calls());
    assert!(!s.calls().contains("auth login"));
    let out = s.repl_off(&["/integrations status", "/git diff", "/exit"]);
    assert!(out.contains("github.com/base/project"), "{out}");
    let out = s.repl_off(&["/permissions deny gh", "/integrations prs", "/git push", "/exit"]);
    assert!(out.contains("refused by permissions"), "{out}");
    assert!(out.contains("native Git views"), "{out}");
    assert_eq!(s.calls().lines().filter(|l| l.starts_with("gh pr list")).count(), 1);
}

#[test]
fn native_integrations_support_self_hosted_gitlab_and_fail_closed_for_missing_bitbucket_auth() {
    let s = Sandbox::new("native-gitlab");
    integration_repo(&s);
    assert!(Command::new("git")
        .args(["remote", "set-url", "origin", "ssh://git@git.company.test/group/sub/repo.git"])
        .current_dir(&s.project)
        .status()
        .unwrap()
        .success());
    s.fake("glab", "printf '%s\\n' '[{\"iid\":3,\"source_branch\":\"feature\",\"target_branch\":\"main\"}]'");
    let out = s.cmd().args(["--integrations", "prs"]).output().unwrap();
    assert!(!out.status.success());
    let out = s.cmd().args(["--integrations", "configure", "gitlab"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = s.cmd().args(["--integrations", "prs"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        s.calls().contains("glab mr list --repo https://git.company.test/group/sub/repo --per-page 20 --output json"),
        "{}",
        s.calls()
    );
    let out = s.cmd().args(["--integrations", "setup"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("glab auth login --hostname 'git.company.test'"));
    assert!(Command::new("git")
        .args(["remote", "set-url", "origin", "git@bitbucket.org:workspace/repo.git"])
        .current_dir(&s.project)
        .status()
        .unwrap()
        .success());
    let out = s.cmd().args(["--integrations", "configure", "auto"]).output().unwrap();
    assert!(out.status.success());
    let out = s
        .cmd()
        .env_remove("BITBUCKET_ACCESS_TOKEN")
        .env_remove("BITBUCKET_EMAIL")
        .env_remove("BITBUCKET_API_TOKEN")
        .args(["--integrations", "check"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("BITBUCKET_ACCESS_TOKEN"));
    let out = s.cmd().args(["--integrations", "configure", "unknown"]).output().unwrap();
    assert!(!out.status.success());
}

#[test]
fn native_integration_auth_failures_are_redacted_and_credentials_are_never_persisted() {
    let s = Sandbox::new("native-auth-failure");
    integration_repo(&s);
    s.fake("gh", "echo 'NVIDIA_API_KEY=nvapi-abcdefghijklmnop1234567890' >&2; exit 1");
    let out = s.cmd().args(["--integrations", "check"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("nvapi-abcdefghijklmnop1234567890"), "{stderr}");
    assert!(s.calls().contains("gh auth status --hostname github.com"));
    assert!(!s.calls().contains("auth login"));
    let out = s.cmd().args(["--integrations", "configure", "github", "origin"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut files = Vec::new();
    find_files(&s.home, "json", &mut files);
    let saved = files.iter().find(|p| p.file_name().unwrap() == "integrations.json").unwrap();
    let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(saved).unwrap()).unwrap();
    assert_eq!(config, serde_json::json!({"provider":"github", "remote":"origin"}));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(saved.metadata().unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn native_integrations_refuse_malformed_permission_rules_before_host_or_config_actions() {
    let s = Sandbox::new("native-invalid-policy");
    integration_repo(&s);
    s.fake("gh", "exit 0");
    let out = s.cmd().args(["--integrations", "configure", "github"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut files = Vec::new();
    find_files(&s.home, "json", &mut files);
    let config = files.iter().find(|p| p.file_name().unwrap() == "integrations.json").unwrap();
    std::fs::write(config.with_file_name("permissions.json"), "{").unwrap();
    for args in [vec!["--integrations", "check"], vec!["--integrations", "mcp", "add", "blocked", "/bin/true"]] {
        let out = s.cmd().args(args).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("permissions"));
    }
    assert!(!s.calls().contains("gh auth"));
    assert!(!s.project.join(".mcp.json").exists());
}

#[test]
fn unified_mcp_setup_preserves_other_servers_and_never_launches_on_add_or_list() {
    let s = Sandbox::new("native-mcp-setup");
    let path = s.project.join(".mcp.json");
    std::fs::write(&path, r#"{"mcpServers":{"existing":{"command":"unavailable-existing-server","env":{"TOKEN":"keep-this-secret"}}},"custom":{"preserve":true}}"#).unwrap();
    s.fake("new-server", "printf started > launched");
    let out = s
        .cmd()
        .env_remove("NVIDIA_API_KEY")
        .args(["--integrations", "mcp", "add", "new", "new-server", "path with spaces"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(cfg["mcpServers"]["existing"]["env"]["TOKEN"], "keep-this-secret");
    assert_eq!(cfg["custom"]["preserve"], true);
    assert_eq!(cfg["mcpServers"]["new"]["args"][0], "path with spaces");
    assert_eq!(cfg["mcpServers"]["new"]["required"], false);
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"required\": true"));
    let out = s.cmd().args(["--integrations", "mcp", "list"]).output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("existing") && text.contains("new") && !text.contains("keep-this-secret"));
    assert!(!s.project.join("launched").exists());
    assert!(!s.calls().contains("new-server"));
    let out =
        s.cmd().args(["--permissions", "read-only", "--integrations", "mcp", "remove", "existing"]).output().unwrap();
    assert!(!out.status.success());
    let unchanged = std::fs::read(&path).unwrap();
    let out = s.cmd().args(["--integrations", "mcp", "add", "new", "replacement"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), unchanged);
    let out = s.cmd().args(["--integrations", "mcp", "remove", "new"]).output().unwrap();
    assert!(out.status.success());
    let out = s
        .cmd()
        .args(["--integrations", "mcp", "add", "gh", "--", "--env", "GH_TOKEN", "gh-server", "--verbose"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        cfg["mcpServers"]["gh"],
        serde_json::json!({"command":"gh-server","args":["--verbose"],"required":false,"env":{"GH_TOKEN":"${GH_TOKEN}"}})
    );
    let out = s
        .cmd()
        .args(["--integrations", "mcp", "add", "leak", "--", "gh-server", "--token=abcdefghijklmnop"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(!std::fs::read_to_string(&path).unwrap().contains("abcdefghijklmnop"));
}

#[test]
fn unified_mcp_health_check_cannot_mark_an_unavailable_optional_server_healthy() {
    let s = Sandbox::new("native-mcp-health");
    std::fs::write(
        s.project.join(".mcp.json"),
        r#"{"mcpServers":{"broken":{"command":"not-an-installed-mcp-server"}}}"#,
    )
    .unwrap();
    let out = s.cmd().env_remove("NVIDIA_API_KEY").args(["--integrations", "mcp", "check"]).output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("all_connected") && err.contains("start_failed"), "{err}");
    std::fs::write(s.project.join(".mcp.json"), "not json").unwrap();
    let out = s.cmd().args(["--integrations", "mcp", "add", "new", "anything"]).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(s.project.join(".mcp.json")).unwrap(), "not json");
}

#[test]
fn concurrent_mcp_configuration_adds_preserve_every_server_across_profiles() {
    let s = Sandbox::new("native-mcp-concurrent");
    let mut children = Vec::new();
    for n in 0..8 {
        children.push(
            s.cmd()
                .env("RUSTY_HOME", s.home.join(format!("profile-{n}")))
                .env_remove("NVIDIA_API_KEY")
                .args(["--integrations", "mcp", "add", &format!("server{n}"), "/bin/true"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let out = wait(child, Duration::from_secs(30));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(s.project.join(".mcp.json")).unwrap()).unwrap();
    let servers = config["mcpServers"].as_object().unwrap();
    assert_eq!(servers.len(), 8);
    for n in 0..8 {
        assert_eq!(servers[&format!("server{n}")]["command"], "/bin/true");
    }
    assert!(!s.project.join(".mcp.json.pending").exists());
}

// Scripted model replies exercise the real CLI and shell without provider spend.
fn pinned_completion(case: &str, replies: Vec<serde_json::Value>) -> (Output, serde_json::Value) {
    let s = Sandbox::new(&format!("pinned-{case}"));
    seed_calc(&s);
    std::fs::write(s.project.join("check.py"), "from calc import add\nassert add(2, 3) == 5\n").unwrap();
    let (url, server) = scripted_endpoint(replies);
    let out = wait(
        s.cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_GOAL_MAX_TURNS", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args([
                "--mode",
                "standard",
                "--agents",
                "off",
                "--memory",
                "off",
                "--yolo",
                "--goal",
                "repair calc.py",
                "--verify",
                "python3 check.py",
                "--verify-file",
                "check.py",
                "--stats",
                "--trajectory",
            ])
            .arg(s.home.join("trace.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(15),
    );
    let trace = serde_json::from_str(&std::fs::read_to_string(s.home.join("trace.json")).unwrap()).unwrap();
    server.join().unwrap();
    (out, trace)
}

#[test]
fn completion_contract_pinned_checker_accepts_repair() {
    let (out, trace) = pinned_completion("repair", vec![fix_calc(), done_reply()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    assert_eq!(trace["goal_outcome"], "verified");
}

#[test]
fn completion_contract_pinned_checker_rejects_tampering() {
    for (case, command) in [
        ("overwrite", "printf 'pass\\n' > check.py"),
        ("delete", "rm check.py"),
        ("symlink", "mv check.py original.py; ln -s original.py check.py"),
    ] {
        let (out, trace) = pinned_completion(case, vec![bash_reply(command), done_reply(), text_reply("unresolved")]);
        assert_eq!(out.status.code(), Some(2), "{case}");
        assert_eq!(trace["claims"]["accepted"], 0, "{case}: {trace}");
        assert_eq!(trace["claims"]["check_error"], 1, "{case}: {trace}");
        assert!(trace["messages"].to_string().contains("pinned verification"), "{trace}");
    }
}

#[test]
fn completion_contract_pinned_checker_can_be_restored_then_rechecked() {
    let (out, trace) = pinned_completion(
        "restore",
        vec![
            bash_reply("cp check.py original.py; printf 'pass\\n' > check.py"),
            done_reply(),
            bash_reply("cp original.py check.py"),
            fix_calc(),
            done_reply(),
        ],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(trace["claims"]["check_error"], 1);
    assert_eq!(trace["claims"]["accepted"], 1);
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    assert_eq!(trace["goal_outcome"], "verified");
}

#[test]
fn completion_contract_unpinned_checker_is_not_independent_proof() {
    let s = Sandbox::new("unpinned-check-control");
    seed_calc(&s);
    std::fs::write(s.project.join("check.py"), "from calc import add\nassert add(2, 3) == 5\n").unwrap();
    let (out, trace, _) = verified_run(
        &s,
        "python3 check.py",
        vec![bash_reply("printf 'pass\\n' > check.py"), done_reply()],
        "standard",
        "yolo",
    );
    assert!(out.status.success());
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    let independent = Command::new("python3")
        .current_dir(&s.project)
        .args(["-B", "-c", "from calc import add; assert add(2, 3) == 5"])
        .output()
        .unwrap();
    assert!(!independent.status.success(), "the unchanged broken implementation must fail independent grading");
}

#[test]
fn completion_contract_pagination_grader_rejects_plausible_wrong_repairs() {
    let s = Sandbox::new("pagination-grader");
    let task = Path::new(env!("CARGO_MANIFEST_DIR")).join("evals/verified-pagination");
    let oracle = "def collect(fetch, start=None):\n    items = []\n    seen = set()\n    cursor = start\n    while True:\n        if cursor in seen:\n            raise ValueError('cycle')\n        seen.add(cursor)\n        page = fetch(cursor)\n        items.extend(page['items'])\n        cursor = page['next']\n        if cursor is None:\n            return items\n";
    let original = std::fs::read_to_string(task.join("files/pagination.py")).unwrap();
    for (name, body, passes) in [
        ("original", original, false),
        ("oracle", oracle.to_string(), true),
        ("partial", oracle.replace("raise ValueError('cycle')", "return items"), false),
        ("duplicates", oracle.replace("return items", "return list(dict.fromkeys(items))"), false),
        ("ignores-start", oracle.replace("cursor = start", "cursor = None"), false),
    ] {
        std::fs::write(s.project.join("pagination.py"), body).unwrap();
        let out =
            Command::new("python3").arg("-B").arg(task.join("hidden.py")).current_dir(&s.project).output().unwrap();
        assert_eq!(out.status.success(), passes, "{name}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[test]
fn completion_contract_pinned_checker_survives_real_session_resume() {
    let s = Sandbox::new("pinned-resume");
    seed_calc(&s);
    std::fs::write(s.project.join("check.py"), "from calc import add\nassert add(2, 3) == 5\n").unwrap();
    let (url, server) =
        scripted_endpoint(vec![tool_reply("goal_done", serde_json::json!({"blocked":true,"evidence":"need input"}))]);
    let out = wait(
        s.cmd()
            .env("RUSTY_BASE_URL", url)
            .args([
                "--mode",
                "standard",
                "--agents",
                "off",
                "--memory",
                "off",
                "--yolo",
                "--goal",
                "repair calc.py",
                "--verify",
                "python3 -B check.py",
                "--verify-file",
                "check.py",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(out.status.code(), Some(2));
    server.join().unwrap();
    std::fs::write(s.project.join("check.py"), "pass\n").unwrap();
    let (url, server) = scripted_endpoint(vec![done_reply(), text_reply("unresolved")]);
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "1")
        .args([
            "--mode",
            "standard",
            "--agents",
            "off",
            "--memory",
            "off",
            "--yolo",
            "--continue",
            "--stats",
            "--trajectory",
        ])
        .arg(s.home.join("resumed.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/goal resume\n/exit").unwrap();
    let out = wait(child, Duration::from_secs(20));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    server.join().unwrap();
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("resumed.json")).unwrap()).unwrap();
    assert_eq!(trace["claims"]["accepted"], 0);
    assert_eq!(trace["claims"]["check_error"], 1);
    assert!(trace["messages"].to_string().contains("pinned verification file changed"));
    assert_eq!(trace["goal"]["status"], "Active");
}

#[test]
fn completion_contract_goal_outcomes_without_a_checker() {
    for (case, reply, code, outcome) in [
        ("claim", done_reply(), 0, "completed_unverified"),
        ("blocked", tool_reply("goal_done", serde_json::json!({"blocked":true,"evidence":"need input"})), 2, "blocked"),
        ("turn-limit", text_reply("still investigating"), 2, "incomplete"),
    ] {
        let s = Sandbox::new(&format!("outcome-{case}"));
        let (url, server) = scripted_endpoint(vec![reply]);
        let path = s.home.join("trace.json");
        let out = wait(
            s.cmd()
                .env("RUSTY_BASE_URL", url)
                .env("RUSTY_GOAL_MAX_TURNS", "1")
                .args(["--mode", "standard", "--memory", "off", "--goal", "inspect", "--stats", "--trajectory"])
                .arg(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            Duration::from_secs(15),
        );
        assert_eq!(out.status.code(), Some(code), "{case}: {}", String::from_utf8_lossy(&out.stderr));
        let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(trace["goal_outcome"], outcome);
        assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("\"goal_outcome\":\"{outcome}\"")));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[test]
fn completion_contract_executed_check_retains_receipt_when_pin_changes() {
    let s = Sandbox::new("check-changes-pin");
    std::fs::write(s.project.join("criterion.txt"), "original").unwrap();
    let (url, server) = scripted_endpoint(vec![done_reply(), text_reply("cannot finish")]);
    let path = s.home.join("trace.json");
    let out = wait(
        s.cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_GOAL_MAX_TURNS", "1")
            .args([
                "--mode",
                "standard",
                "--memory",
                "off",
                "--yolo",
                "--goal",
                "inspect",
                "--verify",
                "printf changed > criterion.txt; printf receipt-marker",
                "--verify-file",
                "criterion.txt",
                "--trajectory",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(15),
    );
    assert_eq!(out.status.code(), Some(2));
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(trace["verification"].as_array().unwrap().len(), 1);
    let receipt = &trace["verification"][0];
    assert_eq!(receipt["outcome"], "EvidenceUnavailable");
    assert_eq!(receipt["exit_code"], 0);
    assert!(receipt["output"].as_str().unwrap().contains("receipt-marker"));
    assert!(receipt["observation_error"].as_str().unwrap().contains("criterion.txt"));
    assert_eq!(trace["claims"]["check_failed"], 1);
    assert_eq!(trace["claims"]["check_error"], 0);
    assert_eq!(trace["goal_outcome"], "incomplete");
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn completion_contract_interactive_goal_uses_user_fixed_pinned_check() {
    let s = Sandbox::new("interactive-fixed-goal");
    seed_calc(&s);
    std::fs::write(s.project.join("check.py"), "from calc import add\nassert add(2,3)==5\n").unwrap();
    let (url, server) = scripted_endpoint(vec![fix_calc(), done_reply()]);
    let path = s.home.join("trace.json");
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .args(["--mode", "standard", "--memory", "off", "--yolo", "--trajectory"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.take().unwrap(),
        "/goal --verify --timeout 7 python3 check.py\n/goal --pin check.py\n/goal repair calc.py\n/goal status\n/exit"
    )
    .unwrap();
    let out = wait(child, Duration::from_secs(15));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(trace["goal_outcome"], "verified");
    assert_eq!(trace["verification"][0]["outcome"], "Passed");
    assert_eq!(trace["goal"]["check"]["timeout_secs"], 7);
    assert!(String::from_utf8_lossy(&out.stdout).contains("verified:"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn goal_allowance_resets_only_on_explicit_invocation_and_checkpoints_limits() {
    for limit in [0usize, 1, 2] {
        let s = Sandbox::new(&format!("goal-allowance-{limit}"));
        std::fs::write(s.project.join("check.py"), "assert True\n").unwrap();
        let (url, server) = scripted_endpoint(vec![text_reply("Work remains open."); limit * 2]);
        let first = s.home.join("first.json");
        let mut child = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_GOAL_MAX_TURNS", limit.to_string())
            .env("RUSTY_RETRY_SECS", "1")
            .args(["--mode", "standard", "--agents", "off", "--memory", "off", "--yolo", "--trajectory"])
            .arg(&first)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(
            child.stdin.take().unwrap(),
            "/goal --verify python3 -B check.py\n/goal --pin check.py\n/goal inspect this fixture\n/goal resume\n/exit"
        )
        .unwrap();
        let out = wait(child, Duration::from_secs(20));
        assert!(out.status.success(), "limit {limit}: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(server.join().unwrap().len(), limit * 2, "actual requests at limit {limit}");
        let before: serde_json::Value = serde_json::from_slice(&std::fs::read(first).unwrap()).unwrap();
        assert_eq!(before["goal"]["turns"], limit * 2);
        assert_eq!(before["goal"]["run_turns"], limit);
        assert_eq!(before["goal"]["run_limit"], limit);
        assert_eq!(before["goal"]["stop_reason"], "turn_limit");
        assert_eq!(before["goal_end"], "turn_limit");
        assert_eq!(before["goal_outcome"], "incomplete");

        let (url, server) = scripted_endpoint(vec![text_reply("Still working."); limit]);
        let second = s.home.join("second.json");
        let mut child = s
            .cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_GOAL_MAX_TURNS", limit.to_string())
            .env("RUSTY_RETRY_SECS", "1")
            .args(["--mode", "standard", "--agents", "off", "--memory", "off", "--yolo", "--continue", "--trajectory"])
            .arg(&second)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "/goal status\n/goal resume\n/exit").unwrap();
        let out = wait(child, Duration::from_secs(20));
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(server.join().unwrap().len(), limit, "restart never replays a goal before explicit resume");
        let shown = String::from_utf8_lossy(&out.stdout);
        assert!(shown.contains("last stop: turn limit") && shown.contains("lifetime turns"), "{shown}");
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(second).unwrap()).unwrap();
        assert_eq!(after["goal"]["turns"], limit * 3);
        assert_eq!(after["goal"]["run_turns"], limit);
        assert_eq!(after["goal"]["run_limit"], limit);
        assert_eq!(after["goal"]["stop_reason"], "turn_limit");
        assert_eq!(after["goal"]["check"], before["goal"]["check"], "resume retains pinned acceptance");
        assert!(after["verification"].as_array().unwrap().is_empty());
        let path = std::fs::read_dir(sessions_dir(&s))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "json") && p.file_stem().unwrap().len() == 32)
            .unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["goal"], after["goal"], "checkpoint includes final counters and stop transition");
    }
}

#[test]
fn goal_allowance_does_not_reset_a_finite_resource_budget() {
    let s = Sandbox::new("goal-allowance-budget");
    let ledger = s.home.join("ledger.json");
    let (url, server) = scripted_endpoint(vec![text_reply("Keep working."); 3]);
    let path = s.home.join("trace.json");
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .env("RUSTY_GOAL_MAX_TURNS", "2")
        .env("RUSTY_BUDGET_LEDGER", &ledger)
        .env("RUSTY_RETRY_SECS", "1")
        .args([
            "--mode",
            "standard",
            "--agents",
            "off",
            "--memory",
            "off",
            "--yolo",
            "--max-requests",
            "3",
            "--trajectory",
        ])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "/goal inspect this fixture\n/goal resume\n/exit").unwrap();
    let out = wait(child, Duration::from_secs(20));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(server.join().unwrap().len(), 3, "resume cannot buy another resource allowance");
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(trace["goal"]["turns"], 4, "the denied partial turn is still attempted");
    assert_eq!(trace["goal"]["run_turns"], 2);
    assert_eq!(trace["goal"]["stop_reason"], "error");
    let ledger: serde_json::Value = serde_json::from_slice(&std::fs::read(ledger).unwrap()).unwrap();
    assert_eq!(ledger["requests"], 3);
    assert!(ledger["denied_requests"].as_u64().unwrap() >= 1);
}

#[test]
fn completion_contract_planted_python_bytecode_cannot_verify_broken_source() {
    let s = Sandbox::new("pinned-bytecode");
    seed_calc(&s);
    std::fs::write(s.project.join("check.py"), "from calc import add\nassert add(2,3)==5\n").unwrap();
    let planted = Command::new("python3").current_dir(&s.project).args(["-B", "-c", r#"
import importlib.util, marshal, pathlib, struct, sys
sys.pycache_prefix = None
source = pathlib.Path('calc.py')
cache = pathlib.Path(importlib.util.cache_from_source(str(source)))
cache.parent.mkdir(exist_ok=True)
st = source.stat()
cache.write_bytes(importlib.util.MAGIC_NUMBER + struct.pack('<III', 0, int(st.st_mtime), st.st_size) + marshal.dumps(compile('def add(a, b): return a + b', 'calc.py', 'exec')))
"#]).status().unwrap();
    assert!(planted.success());
    let control = Command::new("python3")
        .current_dir(&s.project)
        .args(["-B", "-c", "import sys; sys.pycache_prefix=None; from calc import add; assert add(2,3)==5"])
        .status()
        .unwrap();
    assert!(control.success(), "negative control must load the planted bytecode");
    let before = std::fs::read(s.project.join("calc.py")).unwrap();
    let (url, server) = scripted_endpoint(vec![done_reply(), text_reply("still broken")]);
    let path = s.home.join("trace.json");
    let out = wait(
        s.cmd()
            .env("RUSTY_BASE_URL", url)
            .env("RUSTY_GOAL_MAX_TURNS", "1")
            .args([
                "--mode",
                "standard",
                "--agents",
                "off",
                "--memory",
                "off",
                "--yolo",
                "--goal",
                "repair calc",
                "--verify",
                "python3 -B check.py",
                "--verify-file",
                "check.py",
                "--trajectory",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(20),
    );
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(trace["claims"]["accepted"], 0);
    assert_eq!(trace["verification"][0]["outcome"], "Failed");
    assert_eq!(trace["verification"][0]["exit_code"], 1);
    assert_eq!(std::fs::read(s.project.join("calc.py")).unwrap(), before);
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn completion_contract_verify_and_pin_words_remain_ordinary_objectives() {
    for objective in ["verify the checkout flow works", "pin the release version"] {
        let s = Sandbox::new("ordinary-goal-prefix");
        let (url, server) = scripted_endpoint(vec![done_reply()]);
        let path = s.home.join("trace.json");
        let mut cmd = s.cmd();
        cmd.env("RUSTY_BASE_URL", url).args(["--memory", "off", "--mode", "standard", "--trajectory"]).arg(&path);
        let out = s.repl_with(cmd, &[&format!("/goal {objective}"), "/exit"]);
        let trace: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(trace["goal"]["objective"], objective);
        assert!(trace["goal"]["check"].is_null());
        assert_eq!(trace["goal_outcome"], "completed_unverified");
        assert!(!out.contains("fixed check set"));
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[test]
fn ctrl_c_stops_a_workspace_search_and_reaps_its_process() {
    let s = Sandbox::new("query-interrupt");
    s.fake("rg", "echo $$ > query.pid; exec sleep 30");
    let (url, server) = scripted_endpoint(vec![tool_reply("search", serde_json::json!({"pattern":"needle"}))]);
    let mut child = s
        .cmd()
        .env("RUSTY_BASE_URL", url)
        .args([
            "--mode",
            "standard",
            "--agents",
            "off",
            "--memory",
            "off",
            "--yolo",
            "--goal",
            "investigate the workspace",
            "--verify",
            "true",
            "--trajectory",
        ])
        .arg(s.home.join("query-interrupted.json"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert_eq!(server.join().unwrap().len(), 1);
    let started = Instant::now();
    let query_pid = loop {
        if let Some(pid) =
            std::fs::read_to_string(s.project.join("query.pid")).ok().and_then(|text| text.trim().parse::<i32>().ok())
        {
            break pid;
        }
        if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the scripted search did not start");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(unsafe { libc::kill(child.id() as i32, libc::SIGINT) }, 0);
    let out = wait(child, Duration::from_secs(10));
    assert_eq!(out.status.code(), Some(130), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(unsafe { libc::kill(query_pid, 0) }, -1, "query process must be reaped");
    let trace: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.home.join("query-interrupted.json")).unwrap()).unwrap();
    let messages = trace["messages"].to_string();
    assert!(messages.contains("termination=cancelled") && messages.contains("scan_complete=false"), "{messages}");
    assert_eq!(trace["verification"].as_array().unwrap().len(), 0);
}
