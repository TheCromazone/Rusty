//! Every mode combination, end to end through the real binary, offline.
//!
//! A fake model answers by role instead of by script, so one fixture drives
//! every combination: the lead delegates when it can (swarm, else task), or
//! changes a file; workers share a finding and report; the careful checker
//! approves; a goal closes with goal_done, a prompt with a plain answer.
//! Each run then checks what that combination promises.

mod support;

use serde_json::{json, Value};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use support::{serve, Request, Response, TempDir, RUSTY};

/// What the fake model saw, one entry per request.
type Seen = Arc<Mutex<Vec<Value>>>;

fn reply(delta: Value) -> Response {
    let finish = if delta.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
    let chunk = json!({"choices": [{"delta": delta, "finish_reason": finish}],
                       "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
    Response { status: 200, kind: "text/event-stream", body: format!("data: {chunk}\n\ndata: [DONE]\n\n").into_bytes() }
}

fn call(name: &str, args: Value) -> Response {
    reply(json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function",
        "function": {"name": name, "arguments": args.to_string()}}]}))
}

fn say(text: &str) -> Response {
    reply(json!({"content": text}))
}

fn tools(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["function"]["name"].as_str())
        .map(String::from)
        .collect()
}

fn system(body: &Value) -> &str {
    body["messages"][0]["content"].as_str().unwrap_or("")
}

fn is_worker(body: &Value) -> bool {
    system(body).contains("research worker")
}

fn is_review(body: &Value) -> bool {
    is_worker(body) && body["messages"].to_string().contains("Check this proposed completion")
}

/// The role-aware model.
fn model(seen: Seen) -> support::Handler {
    Arc::new(move |req: Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        seen.lock().unwrap().push(body.clone());
        let offered = tools(&body);
        if offered.is_empty() {
            return say("Summary of the work so far.");
        }
        if is_review(&body) {
            return say("Checked the change against the request; nothing to fix.");
        }
        if is_worker(&body) {
            let shared = body["messages"].to_string().contains("\"name\":\"share\"");
            if offered.iter().any(|t| t == "share") && !shared {
                return call("share", json!({"finding": "config lives in settings.toml:3"}));
            }
            return say("Report: inspected my slice; no problems.");
        }
        // A fresh request from the user (or a new goal) gets work; rusty's own
        // notes and the checker's review get a close.
        let last = body["messages"].as_array().and_then(|m| m.last()).cloned().unwrap_or(Value::Null);
        let text = last["content"].as_str().unwrap_or("");
        let fresh = last["role"] == "user"
            && (!text.starts_with("[from rusty") || text.contains("New goal:"))
            && !text.contains("checker's review");
        if fresh {
            if offered.iter().any(|t| t == "swarm") {
                let tasks: Vec<_> = (0..3)
                    .map(|i| json!({"description": format!("slice {i}"), "prompt": format!("inspect part {i}")}))
                    .collect();
                return call("swarm", json!({ "tasks": tasks }));
            }
            if offered.iter().any(|t| t == "task") {
                return call("task", json!({"description": "inspect", "prompt": "inspect the project"}));
            }
            return call("bash", json!({"command": "printf ok > done.txt"}));
        }
        if offered.iter().any(|t| t == "goal_done") {
            return call("goal_done", json!({"evidence": "inspected and verified"}));
        }
        say("All done.")
    })
}

struct Run {
    out: Output,
    seen: Vec<Value>,
    dir: TempDir,
}

impl Run {
    fn stats(&self) -> Value {
        let err = String::from_utf8_lossy(&self.out.stderr);
        err.lines().rev().find_map(|l| serde_json::from_str(l).ok()).unwrap_or(Value::Null)
    }
    fn workers(&self) -> Vec<&Value> {
        self.seen.iter().filter(|b| is_worker(b) && !is_review(b)).collect()
    }
    fn reviews(&self) -> usize {
        self.seen.iter().filter(|b| is_review(b)).count()
    }
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.out.stdout).into_owned()
    }
}

fn run(name: &str, args: &[&str], stdin: Option<&str>) -> Run {
    let seen: Seen = Arc::default();
    let server = serve(model(seen.clone()));
    let dir = TempDir::new(name);
    let (home, project) = (dir.path().join("home"), dir.path().join("project"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("settings.toml"), "[app]\nname = \"demo\"\nport = 8080\n").unwrap();
    let mut cmd = Command::new(RUSTY);
    cmd.current_dir(&project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("RUSTY_HOME", &home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("RUSTY_INFRA", "off")
        .env("NVIDIA_API_KEY", "offline-test-key")
        // Pinned, so a run asks no endpoint for its model list first.
        .env("RUSTY_CONTEXT_TOKENS", "128000")
        .env("RUSTY_BASE_URL", format!("{}/v1", server.url))
        .env("NO_COLOR", "1")
        .args(args)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    if let Some(text) = stdin {
        use std::io::Write;
        child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    let seen = seen.lock().unwrap().clone();
    Run { out, seen, dir }
}

#[test]
fn every_execution_agents_entry_and_permission_combination_works() {
    let mut failures = Vec::new();
    for exec in ["auto", "careful", "standard", "vibe"] {
        for agents in ["off", "sub", "swarm", "auto"] {
            for entry in ["prompt", "goal"] {
                for perms in ["yolo", "auto"] {
                    let name = format!("{exec}-{agents}-{entry}-{perms}");
                    let mut args = vec!["--mode", exec, "--agents", agents, "--permissions", perms, "--stats"];
                    args.extend(if entry == "goal" {
                        ["--goal", "survey the project"]
                    } else {
                        ["--", "survey the project"]
                    });
                    let r = run(&name, &args, None);
                    let problem = check(&r, exec, agents, entry);
                    if let Some(p) = problem {
                        failures.push(format!("{name}: {p}\n  stderr: {}", String::from_utf8_lossy(&r.out.stderr)));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{} combinations failed:\n{}", failures.len(), failures.join("\n"));
}

/// What each combination promises.
fn check(r: &Run, exec: &str, agents: &str, entry: &str) -> Option<String> {
    if !r.out.status.success() {
        return Some(format!("exit {:?}", r.out.status.code()));
    }
    let stats = r.stats();
    if entry == "goal" && stats["goal"] != "done" {
        return Some(format!("goal ended {}", stats["goal"]));
    }
    let lead_tools = tools(&r.seen[0]);
    let (has_task, has_swarm) = (lead_tools.contains(&"task".into()), lead_tools.contains(&"swarm".into()));
    let want = (matches!(agents, "sub" | "auto"), matches!(agents, "swarm" | "auto"));
    if (has_task, has_swarm) != want {
        return Some(format!("delegation tools task={has_task} swarm={has_swarm}, wanted {want:?}"));
    }
    let workers = r.workers();
    let expected_workers = if has_swarm {
        3
    } else if has_task {
        1
    } else {
        0
    };
    // A swarm worker that shares first makes two requests.
    let distinct: std::collections::BTreeSet<String> =
        workers.iter().map(|b| b["messages"][1]["content"].to_string()).collect();
    if distinct.len() != expected_workers {
        return Some(format!("{} workers ran, wanted {expected_workers}", distinct.len()));
    }
    for w in &workers {
        let t = tools(w);
        if t.iter().any(|t| t == "write_file" || t == "edit_file" || t == "swarm" || t == "task") {
            return Some(format!("a worker was offered {t:?}"));
        }
        if t.contains(&"share".into()) != has_swarm {
            return Some("share offered to the wrong workers".into());
        }
    }
    if has_swarm
        && !r.seen.iter().any(|b| !is_worker(b) && b["messages"].to_string().contains("Findings the workers shared"))
    {
        return Some("the lead never saw the shared findings".into());
    }
    // Careful reviews once when something changed or a goal closes; nothing else reviews.
    let changed = expected_workers == 0;
    let reviews = if exec == "careful" && (entry == "goal" || changed) { 1 } else { 0 };
    if r.reviews() != reviews {
        return Some(format!("{} reviews, wanted {reviews}", r.reviews()));
    }
    if changed && !r.dir.path().join("project/done.txt").exists() {
        return Some("the lead's change did not happen".into());
    }
    None
}

#[test]
fn views_change_what_is_shown_not_what_happens() {
    let mut shown = Vec::new();
    for view in ["default", "verbose", "adhd"] {
        let r =
            run(&format!("view-{view}"), &["--view", view, "--agents", "swarm", "--yolo", "--goal", "survey"], None);
        assert!(r.out.status.success(), "{view}: {}", String::from_utf8_lossy(&r.out.stderr));
        assert!(r.workers().len() >= 3, "{view}");
        shown.push((view, r.stdout()));
    }
    let worker_lines = |s: &str| s.lines().filter(|l| l.contains("slice ")).count();
    assert!(worker_lines(&shown[0].1) >= 3, "default shows each worker:\n{}", shown[0].1);
    assert_eq!(worker_lines(&shown[2].1), 0, "adhd hides worker lines:\n{}", shown[2].1);
    assert!(shown[2].1.len() < shown[0].1.len() && shown[0].1.len() <= shown[1].1.len());
}

#[test]
fn loop_runs_the_prompt_each_time() {
    let r = run("loop", &["--yolo"], Some("/loop 1s x2 check the build\n"));
    assert!(r.out.status.success(), "{}", String::from_utf8_lossy(&r.out.stderr));
    let turns = r.seen.iter().filter(|b| !is_worker(b) && b["messages"].as_array().map_or(0, |m| m.len()) > 0).count();
    assert!(turns >= 4, "two runs of a change and an answer, saw {turns} requests");
}

#[test]
fn ask_without_a_terminal_refuses_changes_and_the_turn_still_ends() {
    let r = run("ask", &["--permissions", "ask", "--", "survey the project"], None);
    assert!(r.out.status.success(), "{}", String::from_utf8_lossy(&r.out.stderr));
    assert!(!r.dir.path().join("project/done.txt").exists());
    let last = r.seen.last().unwrap()["messages"].to_string();
    assert!(last.contains("needs approval"), "{last}");
}

#[test]
fn a_swarm_tells_the_lead_which_neighbouring_files_no_worker_opened() {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    // Workers read settings.toml and report; the lead swarms once, then answers.
    let server = serve(Arc::new(move |req: Request| {
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        log.lock().unwrap().push(body.clone());
        let history = body["messages"].to_string();
        if is_worker(&body) {
            if !history.contains("\"name\":\"read_file\"") {
                return call("read_file", json!({"path": "settings.toml"}));
            }
            return say("Report: settings look fine.");
        }
        if !history.contains("\"name\":\"swarm\"") {
            let tasks: Vec<_> =
                (0..2).map(|i| json!({"description": format!("part {i}"), "prompt": "check the settings"})).collect();
            return call("swarm", json!({ "tasks": tasks }));
        }
        say("All done.")
    }));
    let dir = TempDir::new("coverage");
    let (home, project) = (dir.path().join("home"), dir.path().join("project"));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("settings.toml"), "[app]\nport = 8080\n").unwrap();
    std::fs::write(project.join("secrets.toml"), "[db]\nhost = \"db\"\n").unwrap();
    let out = Command::new(RUSTY)
        .current_dir(&project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("RUSTY_HOME", &home)
        .env("RUSTY_NO_DOTENV", "1")
        .env("RUSTY_INFRA", "off")
        .env("NVIDIA_API_KEY", "offline-test-key")
        // Pinned, so a run asks no endpoint for its model list first.
        .env("RUSTY_CONTEXT_TOKENS", "128000")
        .env("RUSTY_BASE_URL", format!("{}/v1", server.url))
        .env("NO_COLOR", "1")
        .args(["--agents", "swarm", "--mode", "standard", "--yolo", "--", "audit the config"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let seen = seen.lock().unwrap();
    let lead = seen.iter().rev().find(|b| !is_worker(b)).unwrap()["messages"].to_string();
    let section = lead.split("## Not opened by any worker").nth(1).expect("a coverage section");
    let listed = section.split("workers read: ").nth(1).and_then(|r| r.split(". ").next()).unwrap_or("");
    assert_eq!(listed, "secrets.toml", "{section}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 file beside what the workers read went unopened"));
}
