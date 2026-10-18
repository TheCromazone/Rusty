//! The Daytona launcher, offline.
//!
//! Launcher steps run their real shell commands on this machine through a
//! stand-in sandbox with a fake rusty on PATH, so tracking, detaching, export
//! and cleanup are tested end to end without a Daytona account. The
//! `rusty-cloud` binary is then driven against a fake Daytona REST API.

mod support;

use rusty::cloud::launcher::{self, RunState, Runs};
use rusty::cloud::{Remote, Sandboxes};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use support::*;

const FAKE_RUSTY: &str = r#"#!/bin/bash
# Pretends to be rusty: edits a file, adds one, leaves session state behind.
traj=""
while [ $# -gt 0 ]; do [ "$1" = --trajectory ] && traj=$2; shift; done
echo "working on it"
if [ -n "${RUSTY_FAKE_RELEASE:-}" ]; then
    while [ ! -f "$RUSTY_FAKE_RELEASE" ]; do sleep 0.01; done
fi
sleep "${RUSTY_FAKE_SLEEP:-${FAKE_RUSTY_SLEEP:-0}}"
echo 'print("fixed")' > app.py
echo 'print("new")' > new_module.py
mkdir -p "$HOME/.config/rusty/sessions"
echo '{"messages": []}' > "$HOME/.config/rusty/sessions/last.json"
echo 'NVIDIA_API_KEY=nvapi-secret' > "$HOME/.config/rusty/.env"
echo '{"agent": "rusty"}' > "$traj"
echo '{"requests": 3}' >&2
echo "done"
"#;

const FAKE_MEMORYD: &str = "#!/bin/bash\nif [ \"$1\" = export ]; then printf compressed-snapshot > \"$2\"; fi\n";

/// Runs commands locally with HOME pointing at a scratch directory.
struct FakeRemote {
    home: PathBuf,
    bin: PathBuf,
    env: Mutex<BTreeMap<String, String>>,
    labels: Mutex<BTreeMap<String, String>>,
    started: Mutex<Vec<Child>>,
}

impl FakeRemote {
    fn new(root: &Path, run_id: &str) -> Self {
        let home = root.join("home");
        let bin = root.join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        write_exec(&bin.join("rusty"), FAKE_RUSTY);
        write_exec(&bin.join("rusty-memoryd"), FAKE_MEMORYD);
        let labels = BTreeMap::from([("app".into(), "rusty".into()), ("rusty-run".into(), run_id.into())]);
        Self { home, bin, env: Mutex::default(), labels: Mutex::new(labels), started: Mutex::default() }
    }

    fn command(&self, cmd: &str) -> Command {
        let mut c = Command::new("bash");
        c.arg("-c")
            .arg(cmd)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin.display()))
            .envs(self.env.lock().unwrap().clone());
        c
    }

    fn local(&self, path: &str) -> PathBuf {
        PathBuf::from(path.replace("$HOME", self.home.to_str().unwrap()))
    }
}

impl Remote for FakeRemote {
    fn id(&self) -> &str {
        "sbx-1"
    }
    fn labels(&self) -> BTreeMap<String, String> {
        self.labels.lock().unwrap().clone()
    }
    fn sh(&self, cmd: &str, timeout: u64) -> anyhow::Result<(i32, String)> {
        let child = self.command(cmd).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let out = wait_command(child, Duration::from_secs(timeout));
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
        ))
    }
    fn start(&self, cmd: &str, _session: &str) -> anyhow::Result<String> {
        self.started.lock().unwrap().push(self.command(cmd).stdin(Stdio::null()).spawn()?);
        Ok("cmd-1".into())
    }
    fn upload(&self, local: &Path, path: &str) -> anyhow::Result<()> {
        std::fs::copy(local, self.local(path))?;
        Ok(())
    }
    fn download(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.local(path)).ok()
    }
}

impl Drop for FakeRemote {
    fn drop(&mut self) {
        for mut c in self.started.lock().unwrap().drain(..) {
            let _ = wait_child(&mut c);
        }
    }
}

fn wait_child(c: &mut Child) -> Option<()> {
    let start = std::time::Instant::now();
    while c.try_wait().ok()?.is_none() && start.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = c.kill();
    c.wait().ok().map(|_| ())
}

#[derive(Default)]
struct FakeDeleter(Mutex<Vec<String>>);

impl Sandboxes for FakeDeleter {
    fn delete_sandbox(&self, id: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(id.into());
        Ok(())
    }
}

impl FakeDeleter {
    fn deleted(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// Fields drop in order: background commands finish before the directory goes.
struct Fixture {
    remote: FakeRemote,
    runs: Runs,
    state: RunState,
    tmp: TempDir,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let tmp = TempDir::new(name);
        let repo = tmp.path().join("project");
        git_repo(&repo, &[("app.py", "print(\"broken\")\n")]);
        let state = RunState {
            id: "run-1".into(),
            repo: repo.display().to_string(),
            mode: "careful".into(),
            agents: "off".into(),
            sandbox: Some("sbx-1".into()),
            status: "creating".into(),
            ..Default::default()
        };
        let remote = FakeRemote::new(tmp.path(), "run-1");
        Self { runs: Runs { root: tmp.path().join("cloud-runs") }, tmp, state, remote }
    }

    fn start(&mut self, task: &[&str]) {
        let task: Vec<String> = task.iter().map(|s| s.to_string()).collect();
        launcher::setup_and_start(&self.remote, &self.runs, &mut self.state, &task).unwrap();
    }

    fn follow(&mut self) -> (Option<i32>, String) {
        self.follow_until(&|| false)
    }

    fn follow_until(&mut self, stop: &dyn Fn() -> bool) -> (Option<i32>, String) {
        let mut out = Vec::new();
        let code =
            launcher::follow(&self.remote, &self.runs, &mut self.state, Duration::from_millis(50), &mut out, stop)
                .unwrap();
        (code, String::from_utf8(out).unwrap())
    }

    fn finish(&mut self, d: &FakeDeleter) -> i32 {
        launcher::finish(d, &self.remote, &self.runs, &mut self.state, false).unwrap()
    }

    fn dest(&self) -> PathBuf {
        self.runs.dir("run-1")
    }

    fn saved(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.dest().join("run.json")).unwrap()).unwrap()
    }
}

fn tar_names(path: &Path) -> Vec<String> {
    let out = Command::new("tar").arg("tzf").arg(path).output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().lines().map(|l| l.trim_end_matches('/').to_string()).collect()
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_run_is_tracked_exported_then_deleted() {
    let mut f = Fixture::new("tracked");
    f.start(&["--goal", "fix app.py"]);
    let saved = f.saved();
    assert_eq!(saved["status"], "running");
    assert_eq!(saved["cmd_id"], "cmd-1");
    assert!(saved["command"].as_str().unwrap().contains("--mode careful"));
    let (code, out) = f.follow();
    assert_eq!(code, Some(0));
    assert!(out.contains("working on it") && out.contains("done"), "{out}");
    let d = FakeDeleter::default();
    assert_eq!(f.finish(&d), 0);
    let dest = f.dest();
    let patch = std::fs::read_to_string(dest.join("patch.diff")).unwrap();
    assert!(patch.contains("+print(\"fixed\")"));
    assert!(patch.contains("new_module.py"));
    assert_eq!(
        std::fs::read_to_string(dest.join("new-files.txt")).unwrap().split_whitespace().collect::<Vec<_>>(),
        ["new_module.py"]
    );
    assert_eq!(tar_names(&dest.join("new-files.tgz")), ["new_module.py"]);
    let names = tar_names(&dest.join("session.tgz"));
    assert!(names.contains(&".config/rusty/sessions/last.json".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n.ends_with(".env")), "a key file left the sandbox");
    let traj: Value = serde_json::from_str(&std::fs::read_to_string(dest.join("trajectory.json")).unwrap()).unwrap();
    assert_eq!(traj, json!({"agent": "rusty"}));
    assert!(std::fs::read_to_string(dest.join("stderr.log")).unwrap().contains("\"requests\": 3"));
    let manifest: Value = serde_json::from_str(&std::fs::read_to_string(dest.join("manifest.json")).unwrap()).unwrap();
    assert!(manifest["patch.diff"]["bytes"].as_u64().unwrap() > 0);
    assert_eq!(d.deleted(), ["sbx-1"]);
    assert_eq!(f.saved()["status"], "deleted");
    // The patch applies cleanly to a fresh checkout.
    let check = f.tmp.path().join("check");
    assert!(Command::new("git").args(["clone", "-q", &f.state.repo]).arg(&check).status().unwrap().success());
    assert!(Command::new("git")
        .arg("apply")
        .arg(dest.join("patch.diff"))
        .current_dir(&check)
        .status()
        .unwrap()
        .success());
    assert_eq!(std::fs::read_to_string(check.join("new_module.py")).unwrap(), "print(\"new\")\n");
}

#[test]
fn advisor_cloud_import_export_and_private_db_exclusion() {
    let mut f = Fixture::new("advisor");
    let incoming = f.tmp.path().join("incoming.gz");
    std::fs::write(&incoming, "scoped-memory").unwrap();
    f.state.memory_mode = Some("deep".into());
    f.state.memory_input = Some(incoming.display().to_string());
    f.state.project_id = Some("my-project".into());
    f.start(&["fix the file"]);
    assert!(f.state.command.as_deref().unwrap().contains("--memory deep"));
    assert_eq!(std::fs::read(f.remote.home.join("rusty-run/memory-input.json.gz")).unwrap(), b"scoped-memory");
    f.follow();
    let private = f.remote.home.join(".config/rusty/memory");
    std::fs::create_dir_all(&private).unwrap();
    std::fs::write(private.join("memory.sqlite-wal"), "do not archive raw live WAL").unwrap();
    assert!(launcher::export(&f.remote, &f.runs, &mut f.state).unwrap());
    let dest = f.dest();
    assert_eq!(std::fs::read(dest.join("memory.json.gz")).unwrap(), b"compressed-snapshot");
    assert_eq!(mode(&dest), 0o700);
    for item in std::fs::read_dir(&dest).unwrap() {
        let p = item.unwrap().path();
        assert_eq!(mode(&p), 0o600, "{}", p.display());
    }
    assert!(!tar_names(&dest.join("session.tgz")).iter().any(|n| n.contains("/memory/")));
}

#[test]
fn missing_trajectory_preserves_the_sandbox() {
    for memory in [None, Some("learn")] {
        let mut f = Fixture::new("no-trajectory");
        f.state.memory_mode = memory.map(Into::into);
        f.start(&["x"]);
        f.follow();
        std::fs::remove_file(f.remote.home.join("rusty-run/trajectory.json")).unwrap();
        let d = FakeDeleter::default();
        f.finish(&d);
        assert!(d.deleted().is_empty());
        assert_eq!(f.state.exported, Some(false));
    }
}

#[test]
fn unknown_exit_preserves_the_sandbox() {
    for memory in [None, Some("learn")] {
        let mut f = Fixture::new("unknown-exit");
        f.state.memory_mode = memory.map(Into::into);
        f.remote.env.lock().unwrap().insert("FAKE_RUSTY_SLEEP".into(), "1".into());
        f.start(&["x"]);
        let d = FakeDeleter::default();
        f.finish(&d);
        assert!(d.deleted().is_empty());
        assert_eq!(f.state.status, "running");
    }
}

#[test]
fn missing_memory_export_preserves_cloud_sandbox() {
    let mut f = Fixture::new("no-memory-export");
    f.state.memory_mode = Some("learn".into());
    f.start(&["x"]);
    f.follow();
    std::fs::remove_file(f.remote.bin.join("rusty-memoryd")).unwrap();
    let d = FakeDeleter::default();
    f.finish(&d);
    assert!(d.deleted().is_empty());
    assert_eq!(f.state.exported, Some(false));
}

#[test]
fn ctrl_c_only_stops_following() {
    let mut f = Fixture::new("ctrl-c");
    f.remote.env.lock().unwrap().insert("FAKE_RUSTY_SLEEP".into(), "1".into());
    f.start(&["--goal", "fix app.py"]);
    let once = AtomicBool::new(true);
    let (code, first) = f.follow_until(&|| once.swap(false, Ordering::SeqCst));
    assert_eq!(code, None, "following stops, the run doesn't");
    assert_eq!(f.saved()["status"], "running");
    let (code, second) = f.follow();
    assert_eq!(code, Some(0));
    let both = first + &second;
    assert_eq!(both.matches("working on it").count(), 1, "output was repeated after reconnecting");
    assert!(both.contains("done"));
}

#[test]
fn a_failed_export_keeps_the_sandbox() {
    let mut f = Fixture::new("failed-export");
    f.start(&["--goal", "x"]);
    f.follow();
    f.state.base_commit = Some("0".repeat(40)); // unknown commit: the diff fails
    let d = FakeDeleter::default();
    f.finish(&d);
    assert!(d.deleted().is_empty());
    assert_ne!(f.state.exported, Some(true));
}

#[test]
fn only_this_runs_sandbox_is_ever_deleted() {
    let mut f = Fixture::new("labels");
    *f.remote.labels.lock().unwrap() = BTreeMap::from([("app".into(), "something-else".into())]);
    let d = FakeDeleter::default();
    launcher::delete_sandbox(&d, &f.remote, &f.runs, &mut f.state).unwrap();
    assert!(d.deleted().is_empty());
}

#[test]
fn local_exports_are_private() {
    let mut f = Fixture::new("private");
    f.start(&["x"]);
    f.follow();
    launcher::export(&f.remote, &f.runs, &mut f.state).unwrap();
    assert_eq!(mode(&f.dest()), 0o700);
    for p in std::fs::read_dir(f.dest()).unwrap() {
        let p = p.unwrap().path();
        assert_eq!(mode(&p), 0o600, "{}", p.display());
    }
}

#[test]
fn snapshot_name_follows_the_binary_and_resources() {
    let tmp = TempDir::new("snapname");
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    std::fs::write(&a, "one").unwrap();
    std::fs::write(&b, "two").unwrap();
    let name = |p: &Path| launcher::snapshot_name(p, 2, 4, "").unwrap();
    assert!(launcher::is_snapshot_name(&name(&a)));
    assert_eq!(name(&a), name(&a));
    assert_ne!(name(&a), name(&b));
    let old = name(&a);
    std::fs::write(tmp.path().join("rusty-memoryd"), "advisor-v2").unwrap();
    assert_ne!(old, name(&a));
    assert_ne!(launcher::snapshot_name(&a, 2, 4, "").unwrap(), launcher::snapshot_name(&a, 4, 8, "").unwrap());
}

// ------------------------------------------------------ against the API

fn sandbox_machine(tmp: &TempDir) -> std::sync::Arc<Machine> {
    let bin = tmp.path().join("sandbox-bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_exec(&bin.join("rusty"), FAKE_RUSTY);
    write_exec(&bin.join("rusty-memoryd"), FAKE_MEMORYD);
    Machine::new(tmp.path().join("sandbox-home"), &format!("{}:/usr/bin:/bin", bin.display()))
}

fn text(out: &std::process::Output) -> (String, String) {
    (String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into())
}

fn the_only_run(cwd: &Path) -> PathBuf {
    let runs: Vec<_> = std::fs::read_dir(cwd.join("cloud-runs")).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    runs[0].clone()
}

#[test]
fn prepare_fails_before_any_api_call_without_a_binary() {
    let tmp = TempDir::new("no-binary");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    let out = api.cloud(tmp.path()).args(["prepare", "--binary"]).arg(tmp.path().join("missing")).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("not found. Build the static Linux binary"), "{}", text(&out).1);
    assert!(api.requests().is_empty());
    // And no key is needed to find that out; with no key, nothing is called.
    let out = api.cloud(tmp.path()).env_remove("DAYTONA_API_KEY").args(["run", "--repo", "x", "--goal", "y"]).output();
    assert_eq!(out.unwrap().status.code(), Some(1));
    assert!(api.requests().is_empty());
}

#[test]
fn a_cloud_run_through_the_rest_api() {
    let tmp = TempDir::new("api-run");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    api.add_snapshot("rusty-0123456789ab", "active");
    let repo = tmp.path().join("project");
    git_repo(&repo, &[("app.py", "print(\"broken\")\n")]);
    let model_key = "nvapi-model-canary-0123456789";
    let out = api
        .cloud(tmp.path())
        .env("NVIDIA_API_KEY", model_key)
        .env("RUSTY_BASE_URL", format!("{}/v1", api.url))
        .env("RUSTY_HOME", "/local/only")
        .args(["run", "--snapshot", "rusty-0123456789ab", "--goal", "fix app.py", "--mode", "careful", "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stdout.contains("working on it") && stdout.contains("done"), "{stdout}");
    api.machine.join();
    api.assert_ok();

    let dest = the_only_run(tmp.path());
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dest.join("run.json")).unwrap()).unwrap();
    assert_eq!(state["status"], "deleted");
    assert_eq!(state["exit_code"], 0);
    assert_eq!(state["exported"], true);
    assert!(std::fs::read_to_string(dest.join("patch.diff")).unwrap().contains("+print(\"fixed\")"));

    let requests = api.requests();
    let create = requests.iter().find(|r| r.method == "POST" && r.target == "/api/sandbox").expect("sandbox created");
    assert_eq!(create.body["snapshot"], "rusty-0123456789ab");
    assert_eq!(create.body["autoStopInterval"], 0);
    // Keys travel as sandbox variables; local-only settings and Daytona's key never do.
    assert_eq!(create.body["env"], json!({"NVIDIA_API_KEY": model_key, "RUSTY_BASE_URL": format!("{}/v1", api.url)}));
    assert_eq!(create.body["labels"], json!({"app": "rusty", "rusty-run": state["id"]}));
    assert!(requests
        .iter()
        .any(|r| r.method == "DELETE" && r.target == format!("/api/sandbox/{}", state["sandbox"].as_str().unwrap())));
    for r in &requests {
        if r.target.starts_with("/api/") || r.target.starts_with("/toolbox/") {
            assert_eq!(r.auth.as_deref(), Some(format!("Bearer {DAYTONA_KEY}").as_str()), "{}", r.target);
        }
        let command = r.body["command"].as_str().unwrap_or_default();
        assert!(!command.contains(model_key) && !command.contains(DAYTONA_KEY), "a key reached a command line");
    }
    assert!(requests.iter().any(|r| r.target.ends_with("/process/session") && r.body["sessionId"] == "rusty"));

    // The run stays listed after cleanup.
    let out = api.cloud(tmp.path()).arg("list").output().unwrap();
    assert!(text(&out).0.contains("deleted") && text(&out).0.contains("fix app.py"));
    let out = api.cloud(tmp.path()).args(["status", state["id"].as_str().unwrap()]).output().unwrap();
    assert!(text(&out).0.contains("deleted"));
}

#[test]
fn an_unreachable_model_endpoint_stops_before_the_agent_starts() {
    let tmp = TempDir::new("api-unreachable");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    api.add_snapshot("rusty-0123456789ab", "active");
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let out = api
        .cloud(tmp.path())
        .env("NVIDIA_API_KEY", "k")
        .env("RUSTY_BASE_URL", format!("http://{closed}/v1"))
        .args(["run", "--snapshot", "rusty-0123456789ab", "--prompt", "x", "--repo", "/nowhere"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("model endpoint is unreachable"), "{}", text(&out).1);
    let requests = api.requests();
    assert!(!requests.iter().any(|r| r.target.contains("/process/session")), "rusty was started");
    assert_eq!(api.state.lock().unwrap().deleted_sandboxes, ["sbx-1"]);
    // An unknown snapshot is refused before any sandbox exists.
    let out = api
        .cloud(tmp.path())
        .env("NVIDIA_API_KEY", "k")
        .args(["run", "--snapshot", "rusty-ffffffffffff", "--prompt", "x", "--repo", "/nowhere"])
        .output()
        .unwrap();
    assert!(text(&out).1.contains("isn't prepared yet"));
    assert_eq!(api.requests().iter().filter(|r| r.target == "/api/sandbox").count(), 1);
}

#[test]
fn interrupting_a_run_detaches_and_logs_picks_it_up() {
    let tmp = TempDir::new("api-detach");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    struct Gate(PathBuf);
    impl Drop for Gate {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "continue");
        }
    }
    let gate = Gate(api.machine.home.join("fixture-release"));
    api.add_snapshot("rusty-0123456789ab", "active");
    let repo = tmp.path().join("project");
    git_repo(&repo, &[("app.py", "print(\"broken\")\n")]);
    // Keep the remote process alive until detach and status are observed.
    let child = api
        .cloud(tmp.path())
        .env("NVIDIA_API_KEY", "k")
        .env("RUSTY_BASE_URL", format!("{}/v1", api.url))
        .env("RUSTY_FAKE_RELEASE", &gate.0)
        .args(["run", "--snapshot", "rusty-0123456789ab", "--prompt", "x", "--repo"])
        .arg(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    support::sync::until("remote execution and log following", || {
        let requests = api.requests();
        let Some(exec) = requests.iter().position(|r| r.target.ends_with("/exec")) else { return false };
        requests[exec + 1..].iter().any(|r| r.body["command"].as_str().is_some_and(|c| c.contains("tail -c +")))
    });
    unsafe { libc::kill(pid, libc::SIGINT) };
    let out = wait(child, Duration::from_secs(20));
    let (first, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(130), "{stderr}");
    assert!(stderr.contains("rusty-cloud logs"), "{stderr}");
    let dest = the_only_run(tmp.path());
    let id = dest.file_name().unwrap().to_str().unwrap().to_string();
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dest.join("run.json")).unwrap()).unwrap();
    assert_eq!(state["status"], "running");
    let out = api.cloud(tmp.path()).args(["status", &id]).output().unwrap();
    assert!(text(&out).0.contains("running"), "{}", text(&out).0);
    std::fs::write(&gate.0, "continue").unwrap();
    let out = api.cloud(tmp.path()).args(["logs", &id]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out).1);
    let both = first + &text(&out).0;
    assert_eq!(both.matches("working on it").count(), 1, "output was repeated: {both}");
    assert!(both.contains("done"));
    api.machine.join();
    api.assert_ok();
    assert_eq!(api.state.lock().unwrap().deleted_sandboxes, ["sbx-1"]);
}

#[test]
fn prepare_uploads_signed_contexts_and_builds_once() {
    let tmp = TempDir::new("api-prepare");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    let out_dir = tmp.path().join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("rusty"), "static rusty").unwrap();
    std::fs::write(out_dir.join("rusty-memoryd"), "static memoryd").unwrap();
    std::fs::write(tmp.path().join("LICENSE"), "MIT").unwrap();
    std::fs::write(tmp.path().join("THIRD_PARTY_NOTICES.txt"), "notices").unwrap();
    let expected = launcher::snapshot_name(&out_dir.join("rusty"), 2, 4, "").unwrap();
    let out = api.cloud(tmp.path()).arg("prepare").output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert!(stdout.contains(&format!("prepared {expected}")), "{stdout}");
    api.assert_ok();
    let requests = api.requests();
    let create = requests.iter().find(|r| r.method == "POST" && r.target == "/api/snapshots").unwrap();
    assert_eq!(create.body["name"], expected);
    assert_eq!((create.body["cpu"].as_u64(), create.body["memory"].as_u64()), (Some(2), Some(4)));
    let dockerfile = create.body["buildInfo"]["dockerfileContent"].as_str().unwrap();
    assert!(dockerfile.starts_with(&format!("FROM {}\n", rusty::cloud::image::BASE_IMAGE)));
    assert!(dockerfile.contains("COPY out/rusty /usr/local/bin/rusty\n"));
    assert!(dockerfile.contains("COPY out/rusty-memoryd /usr/local/bin/rusty-memoryd\n"));
    assert!(dockerfile.contains("COPY LICENSE /usr/share/doc/rusty/LICENSE\n"));
    assert_eq!(create.body["buildInfo"]["contextHashes"].as_array().unwrap().len(), 4);
    // The binary's context is a tar holding it under its archive name.
    let s3 = api.state.lock().unwrap().s3.clone();
    assert_eq!(s3.len(), 4);
    let mut found = false;
    for data in s3.values() {
        let path = tmp.path().join("ctx.tar");
        std::fs::write(&path, data).unwrap();
        let listing = Command::new("tar").arg("xOf").arg(&path).arg("out/rusty").output().unwrap();
        found |= listing.status.success() && listing.stdout == b"static rusty";
    }
    assert!(found, "out/rusty was not uploaded");
    assert!(stderr.contains("step 1/2"), "the build log was not followed: {stderr}");
    // A second prepare finds it.
    let out = api.cloud(tmp.path()).arg("prepare").output().unwrap();
    assert!(text(&out).0.contains("is already prepared"));
    assert_eq!(api.requests().iter().filter(|r| r.method == "POST").count(), 1);
}

#[test]
fn prepare_source_uploads_only_tracked_runtime_inputs() {
    let tmp = TempDir::new("api-source");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    std::fs::write(tmp.path().join(".env"), "NVIDIA_API_KEY=nvapi-never-uploaded\n").unwrap();
    let out = api.cloud(tmp.path()).args(["prepare", "--source", "--cpu", "4", "--memory", "8"]).output().unwrap();
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    let name = stdout.trim().lines().last().unwrap().to_string();
    assert!(launcher::is_snapshot_name(&name), "{stdout}");
    api.assert_ok();
    let requests = api.requests();
    let create = requests.iter().find(|r| r.method == "POST" && r.target == "/api/snapshots").unwrap();
    assert_eq!(create.body["buildInfo"]["contextHashes"].as_array().unwrap().len(), 7);
    assert_eq!((create.body["cpu"].as_u64(), create.body["memory"].as_u64()), (Some(4), Some(8)));
    assert!(create.body["buildInfo"]["dockerfileContent"].as_str().unwrap().contains("COPY xtask xtask"));
    let state = api.state.lock().unwrap();
    for data in state.s3.values() {
        let text = String::from_utf8_lossy(data);
        assert!(!text.contains("nvapi-never-uploaded"));
        assert!(!text.contains("[core]"), "git metadata was uploaded");
    }
    drop(state);
    // Same sources, same name; an unfinished snapshot is reported, not reused.
    api.state.lock().unwrap().snapshots.get_mut(&name).unwrap().1 = "build_failed".into();
    let out = api.cloud(tmp.path()).args(["prepare", "--source", "--cpu", "4", "--memory", "8"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("inspect before retrying"), "{}", text(&out).1);
}

#[test]
fn prune_lists_then_deletes_only_rusty_snapshots() {
    let tmp = TempDir::new("api-prune");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    api.add_snapshot("rusty-aaaaaaaaaaaa", "active");
    api.add_snapshot("rusty-bbbbbbbbbbbb", "active");
    api.add_snapshot("my-own-snapshot", "active");
    let out = api.cloud(tmp.path()).arg("prune").output().unwrap();
    let stdout = text(&out).0;
    assert!(stdout.contains("would delete rusty-aaaaaaaaaaaa") && stdout.contains("would delete rusty-bbbbbbbbbbbb"));
    assert!(!stdout.contains("my-own") && stdout.contains("dry run"));
    assert!(!api.requests().iter().any(|r| r.method == "DELETE"));
    let out = api.cloud(tmp.path()).args(["prune", "--yes", "--keep", "rusty-aaaaaaaaaaaa"]).output().unwrap();
    assert!(text(&out).0.contains("deleted rusty-bbbbbbbbbbbb"));
    assert_eq!(api.state.lock().unwrap().deleted_snapshots, ["snap-rusty-bbbbbbbbbbbb"]);
    let out = api.cloud(tmp.path()).args(["prune", "--keep", "rusty-aaaaaaaaaaaa"]).output().unwrap();
    assert!(text(&out).0.contains("nothing to prune"));
}

#[test]
fn hybrid_never_creates_or_wakes_a_sandbox() {
    let tmp = TempDir::new("api-hybrid-stopped");
    let api = FakeDaytona::new(sandbox_machine(&tmp));
    api.add_sandbox("stopped-one", "stopped", json!({}));
    let run = |binary: &str| {
        api.cloud(tmp.path())
            .args(["hybrid", "--sandbox", "stopped-one", "--workspace", "/home/daytona/work", "--local-binary", binary])
            .output()
            .unwrap()
    };
    let out = run("/no/such/rusty");
    assert!(text(&out).1.contains("local Rusty binary not found"));
    assert!(api.requests().is_empty(), "validation must come before any API call");
    let out = run(RUSTY);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).1.contains("never creates or wakes"), "{}", text(&out).1);
    let requests = api.requests();
    assert!(requests.iter().all(|r| r.method == "GET"), "hybrid changed something: {requests:?}");
    assert!(!requests.iter().any(|r| r.target.starts_with("/toolbox/")));
}

// ------------------------------------------------- hosted swarm journey

const PATHS: [&str; 3] = ["src/config.rs", "src/signal.rs", "src/execution.rs"];
const DESCS: [&str; 3] = ["agents", "signals", "modes"];
const FACTS: [&str; 3] = [
    "AgentsMode: Off, Sub, Swarm, Auto.",
    "reset() sets INTERRUPTED to false.",
    "ExecutionMode: Careful, Standard, Vibe.",
];
const EXPECTED: [&str; 3] = ["Swarm", "INTERRUPTED", "Careful"];

#[derive(Default)]
struct Metrics {
    entered: usize,
    active: usize,
    peak: usize,
    denied: usize,
    reads: usize,
    problems: Vec<String>,
}

/// The deterministic provider from the hosted smoke: it checks real tool
/// results before advancing.
fn swarm_provider() -> (Server, std::sync::Arc<(Mutex<Metrics>, std::sync::Condvar)>) {
    let shared = std::sync::Arc::new((Mutex::new(Metrics::default()), std::sync::Condvar::new()));
    let s = shared.clone();
    let server = serve(std::sync::Arc::new(move |req: Request| {
        if req.method == "GET" {
            return Response::json(401, json!({}));
        }
        let body = req.json();
        let (prompt, steps, messages) = conversation(&body);
        let last = messages.last().map(content).unwrap_or_default();
        let (lock, cv) = &*s;
        let fail = |m: &str| {
            lock.lock().unwrap().problems.push(m.to_string());
            Response::json(500, json!({"error": m}))
        };
        let tools = body["tools"].to_string();
        if let Some(i) = prompt.strip_prefix("WORKER-").and_then(|r| r[..1].parse::<usize>().ok()) {
            if tools.contains("\"write_file\"") {
                return fail("a worker was offered write_file");
            }
            match steps {
                0 => {
                    let mut m = lock.lock().unwrap();
                    m.entered += 1;
                    m.active += 1;
                    m.peak = m.peak.max(m.active);
                    cv.notify_all();
                    let (mut m, timeout) =
                        cv.wait_timeout_while(m, Duration::from_secs(60), |m| m.entered < 3).unwrap();
                    if timeout.timed_out() {
                        m.problems.push("workers did not overlap".into());
                    }
                    // All three requests are recorded before any returns.
                    m.active -= 1;
                    reply(
                        steps,
                        Some(("write_file", json!({"path": "swarm-worker-must-not-write.txt", "content": "x"}))),
                        "",
                    )
                }
                1 => {
                    if !last.contains("denied") {
                        return fail("a worker write was not denied");
                    }
                    lock.lock().unwrap().denied += 1;
                    reply(steps, Some(("read_file", json!({"path": PATHS[i]}))), "")
                }
                _ => {
                    if !last.contains(EXPECTED[i]) {
                        return fail("file was not read");
                    }
                    lock.lock().unwrap().reads += 1;
                    reply(steps, None, FACTS[i])
                }
            }
        } else {
            match steps {
                0 => {
                    let tasks: Vec<Value> = (0..3)
                        .map(|i| json!({"description": DESCS[i], "prompt": format!("WORKER-{i}: read {} and report the fact.", PATHS[i])}))
                        .collect();
                    reply(steps, Some(("swarm", json!({"tasks": tasks}))), "")
                }
                1 => {
                    if last.matches("(done)").count() != 3 || !FACTS.iter().all(|f| last.contains(f)) {
                        return fail("the swarm report is incomplete");
                    }
                    let report: Vec<String> = (0..3).map(|i| format!("## {}\n{}", DESCS[i], FACTS[i])).collect();
                    reply(
                        steps,
                        Some(("write_file", json!({"path": "swarm-report.md", "content": report.join("\n\n") + "\n"}))),
                        "",
                    )
                }
                2 => reply(
                    steps,
                    Some((
                        "bash",
                        json!({"command": "test -s swarm-report.md && test ! -e swarm-worker-must-not-write.txt && printf verified > swarm-proof.txt"}),
                    )),
                    "",
                ),
                _ => {
                    if !last.contains("exit code: 0") {
                        return fail("verification failed");
                    }
                    reply(steps, None, "Done and verified.")
                }
            }
        }
    }));
    (server, shared)
}

/// The exported results, checked outside rusty's workspace.
fn verify(dest: &Path) -> BTreeMap<&'static str, bool> {
    let read = |n: &str| std::fs::read_to_string(dest.join(n)).unwrap_or_default();
    let trace: Value = serde_json::from_str(&read("trajectory.json")).unwrap_or(json!({}));
    let empty = vec![];
    let messages = trace["messages"].as_array().unwrap_or(&empty);
    let calls: Vec<&Value> = messages
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().map(|c| c.iter().collect()).unwrap_or(vec![]))
        .collect();
    let swarms: Vec<&&Value> = calls.iter().filter(|c| c["function"]["name"] == "swarm").collect();
    let jobs: Vec<usize> = swarms
        .iter()
        .map(|c| {
            let args: Value = serde_json::from_str(c["function"]["arguments"].as_str().unwrap_or("{}")).unwrap();
            args["tasks"].as_array().map_or(0, Vec::len)
        })
        .collect();
    let ids: Vec<&Value> = swarms.iter().map(|c| &c["id"]).collect();
    let reports: Vec<String> =
        messages.iter().filter(|m| m["role"] == "tool" && ids.contains(&&m["tool_call_id"])).map(content).collect();
    let patch = read("patch.diff");
    let manifest: Value = serde_json::from_str(&read("manifest.json")).unwrap_or(json!({}));
    let state: Value = serde_json::from_str(&read("run.json")).unwrap();
    let names: Vec<&str> = calls.iter().filter_map(|c| c["function"]["name"].as_str()).collect();
    let worker_lines = read("out.log")
        .split("· ")
        .skip(1)
        .filter(|s| {
            s.split_once(' ')
                .is_some_and(|(n, rest)| rest.starts_with("tool call") && n.parse::<u32>().is_ok_and(|n| n > 0))
        })
        .count();
    let memory = &trace["memory"];
    BTreeMap::from([
        ("exit", state["exit_code"] == 0),
        ("one_three_worker_swarm", jobs == [3]),
        (
            "all_workers_done",
            reports.len() == 1 && reports[0].matches("(done)").count() == 3 && !reports[0].contains("(failed)"),
        ),
        ("worker_tools", worker_lines >= 3),
        ("lead_write_and_bash", names.contains(&"write_file") && names.contains(&"bash")),
        (
            "report_and_proof",
            patch.contains("diff --git a/swarm-report.md b/swarm-report.md") && patch.contains("+verified"),
        ),
        (
            "expected_facts",
            ["agents", "signals", "modes", "swarm", "careful", "standard", "vibe", "false"]
                .iter()
                .all(|s| patch.to_lowercase().contains(s)),
        ),
        (
            "exports",
            ["patch.diff", "out.log", "stderr.log", "trajectory.json", "session.tgz", "memory.json.gz", "exit"]
                .iter()
                .all(|n| manifest.get(n).is_some()),
        ),
        ("memory_hooks", memory["hooks"].as_u64().unwrap_or(0) > 0 && memory["timeouts"].as_u64() == Some(0)),
        (
            "manifest_integrity",
            manifest.as_object().is_some_and(|m| {
                m.iter().all(|(n, meta)| {
                    std::fs::read(dest.join(n)).is_ok_and(|d| rusty::cloud::digest::sha256_hex(&d) == meta["sha256"])
                })
            }),
        ),
        ("sandbox_deleted_after_export", state["status"] == "deleted" && state["exported"] == true),
    ])
}

const SWARM_PROMPT: &str =
    "Use the swarm tool once with exactly three parallel read-only workers: agents, signals, modes. \
    After collecting all three reports, use write_file to create swarm-report.md, then verify with Bash.";

/// The hosted swarm journey with a scripted provider, offline: the real
/// rusty and rusty-memoryd run in the stand-in sandbox, reached only
/// through the REST API.
#[test]
fn hosted_swarm_journey_offline() {
    let tmp = TempDir::new("api-swarm");
    let bin_dir = Path::new(RUSTY).parent().unwrap();
    let machine = Machine::new(tmp.path().join("sandbox-home"), &format!("{}:/usr/bin:/bin", bin_dir.display()));
    let api = FakeDaytona::new(machine);
    api.add_snapshot("rusty-0123456789ab", "active");
    let repo = tmp.path().join("project");
    git_repo(
        &repo,
        &[
            ("src/config.rs", "pub enum AgentsMode { Off, Sub, Swarm, Auto }\n"),
            ("src/signal.rs", "pub fn reset() { INTERRUPTED.store(false, SeqCst) }\n"),
            ("src/execution.rs", "pub enum ExecutionMode { Careful, Standard, Vibe }\n"),
        ],
    );
    let (provider, metrics) = swarm_provider();
    let out = api
        .cloud(tmp.path())
        .env("RUSTY_API_KEY", "offline-test-key")
        .env("RUSTY_BASE_URL", format!("{}/v1", provider.url))
        .env("RUSTY_NO_DOTENV", "1")
        .args(["run", "--snapshot", "rusty-0123456789ab", "--mode", "standard", "--agents", "swarm"])
        .args(["--memory-mode", "learn", "--prompt", SWARM_PROMPT, "--repo"])
        .arg(&repo)
        .output()
        .unwrap();
    // The memory daemon outlives the agent by design; stop it.
    let _ = Command::new(MEMORYD).arg("--home").arg(tmp.path().join("sandbox-home/.config/rusty")).arg("stop").output();
    let (stdout, stderr) = text(&out);
    api.machine.join();
    let m = metrics.0.lock().unwrap();
    assert!(m.problems.is_empty(), "{:?}\n{stdout}\n{stderr}", m.problems);
    assert_eq!(out.status.code(), Some(0), "{stdout}\n{stderr}");
    assert_eq!((m.peak, m.denied, m.reads), (3, 3, 3), "parallel read-only workers");
    api.assert_ok();
    let checks = verify(&the_only_run(tmp.path()));
    let failed: Vec<_> = checks.iter().filter(|(_, ok)| !**ok).map(|(k, _)| *k).collect();
    assert!(failed.is_empty(), "failed checks {failed:?}\n{stdout}\n{stderr}");
}

/// The same journey against real Daytona with a real model. Needs
/// DAYTONA_API_KEY, model keys, and RUSTY_CLOUD_SNAPSHOT / RUSTY_CLOUD_REF:
///
///     cargo test --test cloud -- --ignored live_swarm_journey
#[test]
#[ignore = "requires Daytona, a sandbox snapshot and live model credentials"]
fn live_swarm_journey() {
    let (Ok(snapshot), Ok(git_ref)) = (std::env::var("RUSTY_CLOUD_SNAPSHOT"), std::env::var("RUSTY_CLOUD_REF")) else {
        panic!("set RUSTY_CLOUD_SNAPSHOT and RUSTY_CLOUD_REF");
    };
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("live-swarm-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let prompt = "Use the swarm tool once with exactly three parallel read-only workers:\n\
        1. description \"agents\": read src/config.rs and list the four AgentsMode variants.\n\
        2. description \"signals\": read src/signal.rs and explain what reset() does to INTERRUPTED.\n\
        3. description \"modes\": read src/execution.rs and list the three ExecutionMode variants.\n\
        Each worker must inspect its file with read_file and give a concise factual report. No worker edits.\n\
        After collecting all three reports, use write_file to create swarm-report.md with three sections titled agents, \
        signals, modes. Include the variants and the reset fact.\n\
        Use Bash to verify all three sections exist, then printf verified > swarm-proof.txt. Do not edit any existing \
        files. Report completion briefly. The swarm call is explicitly requested, even though individual reads are \
        small. No other investigation is needed.";
    let out = Command::new(CLOUD)
        .current_dir(&dir)
        .args(["run", "--snapshot", &snapshot, "--ref", &git_ref, "--repo", "https://github.com/jadenfix/rusty.git"])
        .args(["--mode", "standard", "--agents", "swarm", "--memory-mode", "learn", "--prompt", prompt])
        .status()
        .unwrap();
    let checks = verify(&the_only_run(&dir));
    println!("LIVE_SWARM_RECEIPT {}", serde_json::to_string(&checks).unwrap());
    assert!(out.success() && checks.values().all(|ok| *ok), "{checks:?}");
}
