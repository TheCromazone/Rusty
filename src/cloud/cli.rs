//! `rusty-cloud`: run rusty in a Daytona cloud sandbox.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use super::api::{Config, Daytona};
use super::hybrid::{self, HybridArgs};
use super::image;
use super::launcher::{self, RunState, Runs};
use super::Remote;

const ABOUT: &str = "Run rusty in a Daytona cloud sandbox.

    rusty-cloud prepare                      # once per rusty build
    rusty-cloud run --repo https://github.com/you/project --goal \"get the test suite green\"
    rusty-cloud status|logs|export|stop <run-id>
    rusty-cloud list
    rusty-cloud prune [--yes]                # old rusty snapshots
    rusty-cloud hybrid --sandbox ID --workspace /home/daytona/work

`prepare` builds a Daytona snapshot from a digest-pinned Debian image plus
your exact rusty binary (`docker build --target bin -o out .`), named after
their hash, so every run starts from the same prepared environment.

`run` starts rusty in the background and follows its output. Ctrl-C only
stops following; the run keeps going and `logs`/`status` pick it up again.
When it finishes, rusty's patch, new files, logs, trajectory and session
state are downloaded to cloud-runs/<run-id>/ before the sandbox is deleted.
If the export fails, the sandbox is kept.

Needs DAYTONA_API_KEY plus your model keys (NVIDIA_API_KEY, ...) in the
environment or .env. Keys reach the sandbox as environment variables only.";

#[derive(Parser)]
#[command(name = "rusty-cloud", version, about = "Run rusty in a Daytona cloud sandbox", long_about = ABOUT)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct RunRef {
    run: String,
    /// Keep the sandbox after exporting
    #[arg(long)]
    keep: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Build the pinned snapshot for your rusty binary
    Prepare {
        /// Static Linux rusty binary (default out/rusty)
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Build tracked HEAD in Daytona; no local Docker needed
        #[arg(long)]
        source: bool,
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u8).range(1..=8))]
        cpu: u8,
        /// Snapshot RAM in GiB
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=32))]
        memory: u8,
    },
    /// Start a run
    Run(RunArgs),
    /// Local reasoning/memory + Daytona tools; attach only, no new sandbox
    Hybrid(HybridArgs),
    /// Local reasoning/memory + tools on an existing Linux SSH host
    Ssh(super::ssh::Args),
    /// Is it still working?
    Status(RunRef),
    /// Follow the output, then export
    Logs(RunRef),
    /// Download the results now
    Export(RunRef),
    /// Stop rusty, export, delete
    Stop(RunRef),
    /// Runs started from here
    List,
    /// Delete old rusty-* snapshots (dry run unless --yes)
    Prune {
        #[arg(long)]
        yes: bool,
        /// Also keep this snapshot
        #[arg(long)]
        keep: Option<String>,
    },
}

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("task").required(true).multiple(false))]
struct RunArgs {
    /// Git URL (or path) of the project
    #[arg(long)]
    repo: String,
    /// Branch, tag or commit to start from
    #[arg(long = "ref")]
    git_ref: Option<String>,
    /// Objective for a goal run
    #[arg(long, group = "task")]
    goal: Option<String>,
    /// A single prompt instead of a goal
    #[arg(long, group = "task")]
    prompt: Option<String>,
    #[arg(long, default_value = "auto", value_parser = ["auto", "careful", "standard", "vibe"])]
    mode: String,
    #[arg(long, default_value = "off", value_parser = ["off", "sub", "swarm", "auto"])]
    agents: String,
    /// Model served by the configured endpoint
    #[arg(long)]
    model: Option<String>,
    /// Use this prepared snapshot instead of the one for out/rusty
    #[arg(long)]
    snapshot: Option<String>,
    /// Static Linux rusty binary the snapshot was built from
    #[arg(long)]
    binary: Option<PathBuf>,
    #[arg(long, default_value = "learn", value_parser = ["off", "recall", "learn", "reflect", "deep"])]
    memory_mode: String,
    /// Scoped gzip export from rusty-memoryd
    #[arg(long)]
    memory_input: Option<PathBuf>,
    /// Same RUSTY_PROJECT_ID used locally; defaults to repo URL
    #[arg(long)]
    project_id: Option<String>,
    /// Keep the sandbox after exporting
    #[arg(long)]
    keep: bool,
}

/// Process-wide settings read once at startup.
struct Env {
    /// DAYTONA_API_URL from the real environment, read before any .env.
    api_url: Option<String>,
}

impl Env {
    fn client(&self, target: Option<String>) -> Result<Daytona> {
        let target = target.or_else(|| std::env::var("DAYTONA_TARGET").ok());
        Daytona::new(Config::new(self.api_url.clone(), std::env::var("DAYTONA_API_KEY").ok(), target)?)
    }

    fn target(&self) -> String {
        std::env::var("DAYTONA_TARGET").unwrap_or_default()
    }
}

extern "C" fn on_interrupt(_: libc::c_int) {
    launcher::INTERRUPTED.store(true, Ordering::SeqCst);
}

/// Ctrl-C only stops following a run.
fn catch_interrupt() {
    // SAFETY: installs an async-signal-safe handler that stores one atomic.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

/// Entry point for the `rusty-cloud` binary.
pub fn main() -> i32 {
    // The SDK's rule: the API URL comes only from the real environment, so a
    // project's .env cannot redirect the API key.
    let env = Env { api_url: std::env::var("DAYTONA_API_URL").ok() };
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    match dispatch(&env, cli.command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("☾ {e:#}");
            1
        }
    }
}

fn dispatch(env: &Env, command: Command) -> Result<i32> {
    let runs = Runs::default();
    match command {
        Command::Prepare { binary, source, cpu, memory } => {
            if source {
                prepare_source(env, cpu, memory)
            } else {
                prepare(env, binary.as_deref(), cpu, memory)
            }
        }
        Command::Run(args) => run(env, &runs, args),
        Command::Hybrid(args) => {
            hybrid::command(&args, "daytona")?; // Validate before any API call.
            let d = env.client(None)?;
            let sandbox = d.sandbox(&args.sandbox)?;
            let remote: Arc<dyn Remote> = Arc::new(d.remote(&sandbox)?);
            hybrid::attach(&sandbox.state, remote, &args, &mut hybrid::run_child)
        }
        Command::Ssh(args) => super::ssh::attach(args),
        Command::Status(r) => status(env, &runs, &r.run),
        Command::Logs(r) => {
            let (d, remote, mut state) = reconnect(env, &runs, &r.run)?;
            catch_interrupt();
            let followed = follow(&remote, &runs, &mut state)?;
            if followed.is_none() {
                return Ok(130);
            }
            launcher::finish(&d, &remote, &runs, &mut state, r.keep)
        }
        Command::Export(r) => {
            let (_, remote, mut state) = reconnect(env, &runs, &r.run)?;
            let ok = launcher::export(&remote, &runs, &mut state)?;
            if ok {
                eprintln!("☾ exported to {}", state.export_dir.as_deref().unwrap_or_default());
            } else {
                eprintln!("☾ export failed");
            }
            Ok(if ok { 0 } else { 1 })
        }
        Command::Stop(r) => {
            let (d, remote, mut state) = reconnect(env, &runs, &r.run)?;
            if launcher::exit_code(&remote)?.is_none() {
                remote.sh("pkill -INT -x rusty; sleep 3; pkill -x rusty; true", 60)?;
            }
            launcher::finish(&d, &remote, &runs, &mut state, r.keep)
        }
        Command::List => {
            let all = runs.all();
            if all.is_empty() {
                println!("no runs yet");
            }
            for s in all {
                println!("{}  {:<9}  {}", s.id, s.status, clip(s.task.as_deref().unwrap_or(""), 70));
            }
            Ok(0)
        }
        Command::Prune { yes, keep } => prune(env, yes, keep),
    }
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn follow(remote: &dyn Remote, runs: &Runs, state: &mut RunState) -> Result<Option<i32>> {
    launcher::follow(remote, runs, state, Duration::from_secs(5), &mut std::io::stdout(), &launcher::interrupted)
}

fn reconnect(env: &Env, runs: &Runs, id: &str) -> Result<(Daytona, super::api::Toolbox, RunState)> {
    let state = runs.load(id)?;
    let d = env.client(state.target.clone())?;
    let sandbox = d.sandbox(state.sandbox.as_deref().context("the run never got a sandbox")?)?;
    let remote = d.remote(&sandbox)?;
    Ok((d, remote, state))
}

fn status(env: &Env, runs: &Runs, id: &str) -> Result<i32> {
    let state = runs.load(id)?;
    let mut line = format!("{}  {}  {}", state.id, state.status, clip(state.task.as_deref().unwrap_or(""), 60));
    if state.status == "running" {
        let (_, remote, _) = reconnect(env, runs, id)?;
        match launcher::exit_code(&remote)? {
            None => line.push_str("  (still working)"),
            Some(code) => line.push_str(&format!("  (rusty exited {code}; run `logs` or `stop` to export)")),
        }
    }
    println!("{line}");
    Ok(0)
}

// ----------------------------------------------------------------- prepare

fn log_line(line: &str) {
    eprintln!("{line}");
}

fn snapshot_body(
    name: &str,
    build: &image::Build,
    hashes: Vec<String>,
    cpu: u8,
    memory: u8,
    target: &str,
) -> serde_json::Value {
    let mut body = json!({
        "name": name,
        "buildInfo": {"dockerfileContent": build.dockerfile, "contextHashes": hashes},
        "cpu": cpu,
        "memory": memory,
    });
    if !target.is_empty() {
        body["regionId"] = json!(target);
    }
    body
}

fn prepare(env: &Env, binary: Option<&Path>, cpu: u8, memory: u8) -> Result<i32> {
    let binary = launcher::find_binary(binary)?;
    let advisor = binary.with_file_name("rusty-memoryd");
    if !advisor.is_file() {
        bail!("rusty-memoryd missing; rebuild both binaries with docker build --target bin -o out .");
    }
    let name = launcher::snapshot_name(&binary, cpu, memory, &env.target())?;
    let d = env.client(None)?;
    if d.snapshot(&name)?.is_some() {
        println!("☾ {name} is already prepared");
        return Ok(0);
    }
    let build = image::binary_build(&binary, &advisor)?;
    eprintln!(
        "☾ preparing {name} from {} and {}…",
        image::BASE_IMAGE.split('@').next().unwrap_or(""),
        binary.display()
    );
    let hashes = image::upload_contexts(&d.push_access()?, &build.contexts)?;
    d.create_snapshot(snapshot_body(&name, &build, hashes, cpu, memory, &env.target()), &log_line)?;
    println!("☾ prepared {name}");
    Ok(0)
}

/// The checkout this binary was built from.
fn checkout() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Builds only the tracked runtime inputs in Daytona, never .env or .git.
fn prepare_source(env: &Env, cpu: u8, memory: u8) -> Result<i32> {
    let root = checkout();
    let git = |args: &[&str]| -> Result<Vec<u8>> {
        let out = std::process::Command::new("git").args(args).current_dir(&root).output().context("running git")?;
        if !out.status.success() {
            bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(out.stdout)
    };
    let revision = String::from_utf8(git(&["rev-parse", "HEAD"])?)?.trim().to_string();
    let mut args = vec!["archive", "--format=tar", revision.as_str()];
    args.extend(image::SOURCE_PATHS);
    // git archive contains only our explicit tracked paths, not the checkout.
    let archive = git(&args)?;
    let recipe = image::source_recipe();
    let target = env.target();
    let mut h = super::digest::Sha256::default();
    h.update(&archive);
    h.update(recipe.as_bytes());
    h.update(format!("{cpu}:{memory}").as_bytes());
    h.update(target.as_bytes());
    let name = format!("rusty-{}", &h.hex()[..12]);
    let d = env.client(None)?;
    if let Some(snapshot) = d.snapshot(&name)? {
        if !snapshot.state.eq_ignore_ascii_case("active") {
            bail!("snapshot {name} exists but is {}; inspect before retrying", snapshot.state);
        }
        println!("{name}");
        return Ok(0);
    }
    let tmp = std::env::temp_dir().join(format!("rusty-source-{}", super::digest::random_hex(8)));
    crate::privacy::private_dir(&tmp)?;
    let result = (|| -> Result<()> {
        extract_source_archive(&archive, &tmp)?;
        let build = image::source_build(&tmp)?;
        eprintln!("☾ building {revision} as {name} in Daytona ({cpu} CPU, {memory} GiB)…");
        let hashes = image::upload_contexts(&d.push_access()?, &build.contexts)?;
        d.create_snapshot(snapshot_body(&name, &build, hashes, cpu, memory, &target), &log_line)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    result?;
    println!("{name}");
    Ok(0)
}

fn extract_source_archive(archive: &[u8], dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    // Keep staging beside the destination, never inside the source namespace.
    // Exclusive creation ensures cleanup only owns a directory we created.
    let destination = dir.canonicalize()?;
    let parent = destination.parent().context("source extraction needs a parent directory")?;
    let staging = parent.join(format!("rusty-source-archive-{}", super::digest::random_hex(16)));
    std::fs::DirBuilder::new().mode(0o700).create(&staging)?;
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let cleanup = Cleanup(staging);
    let path = cleanup.0.join("archive.tar");
    let mut file = crate::privacy::private_file(&path, false)?;
    std::io::Write::write_all(&mut file, archive)?;
    drop(file);
    // tar can exit after its end markers without reading all valid zero
    // padding. A regular file avoids racing that exit with a pipe writer.
    let status = std::process::Command::new("tar")
        .arg("xf")
        .arg(&path)
        .arg("-C")
        .arg(&destination)
        .stdin(std::process::Stdio::null())
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("extracting the source archive failed");
    }
    std::fs::remove_dir_all(&cleanup.0).context("removing the source archive staging directory")?;
    Ok(())
}

fn prune(env: &Env, yes: bool, keep: Option<String>) -> Result<i32> {
    let d = env.client(None)?;
    let mut kept: Vec<String> = keep.into_iter().collect();
    if Path::new("out/rusty").is_file() {
        kept.push(launcher::snapshot_name(Path::new("out/rusty"), 2, 4, &env.target())?);
    }
    let old: Vec<String> = d
        .snapshots()?
        .into_iter()
        .map(|s| s.name)
        .filter(|n| launcher::is_snapshot_name(n) && !kept.contains(n))
        .collect();
    if old.is_empty() {
        println!("nothing to prune");
        return Ok(0);
    }
    for name in old {
        if yes {
            // Re-check the name right before deleting; only rusty's own snapshots.
            if let Some(snap) = d.snapshot(&name)? {
                if launcher::is_snapshot_name(&snap.name) {
                    d.delete_snapshot(&snap.id)?;
                    println!("deleted {name}");
                }
            }
        } else {
            println!("would delete {name}");
        }
    }
    if !yes {
        println!("(dry run; pass --yes to delete)");
    }
    Ok(0)
}

// --------------------------------------------------------------------- run

fn run(env: &Env, runs: &Runs, args: RunArgs) -> Result<i32> {
    let vars = launcher::model_env(std::env::vars());
    if !vars.contains_key("NVIDIA_API_KEY") && !vars.contains_key("RUSTY_API_KEY") {
        bail!("set NVIDIA_API_KEY (or RUSTY_API_KEY) in the environment or .env");
    }
    let remembers = crate::advisor::Level::parse(&args.memory_mode).is_some_and(crate::advisor::Level::advises);
    if args.memory_input.is_some() && !remembers {
        bail!("--memory-input requires --memory-mode recall or higher");
    }
    if args.memory_input.as_ref().is_some_and(|p| !p.is_file()) {
        bail!("memory input file not found");
    }
    let snapshot = match &args.snapshot {
        Some(s) => s.clone(),
        None => launcher::snapshot_name(&launcher::find_binary(args.binary.as_deref())?, 2, 4, &env.target())?,
    };
    let d = env.client(None)?;
    if d.snapshot(&snapshot)?.is_none() {
        bail!("snapshot {snapshot} isn't prepared yet. Run:\n  rusty-cloud prepare");
    }
    let id = format!("{}{}", launcher::local_time("%Y%m%d-%H%M%S-"), super::digest::random_hex(2));
    let mut state = RunState {
        id: id.clone(),
        repo: args.repo.clone(),
        git_ref: args.git_ref.clone(),
        snapshot: Some(snapshot.clone()),
        target: d.target().map(Into::into),
        mode: args.mode.clone(),
        agents: args.agents.clone(),
        model: args.model.clone(),
        memory_mode: Some(args.memory_mode.clone()),
        memory_input: args
            .memory_input
            .as_ref()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()).display().to_string()),
        project_id: Some(args.project_id.clone().unwrap_or_else(|| args.repo.clone())),
        task: args.goal.clone().or_else(|| args.prompt.clone()),
        started: Some(launcher::local_time("%Y-%m-%dT%H:%M:%S%z")),
        status: "creating".into(),
        ..Default::default()
    };
    eprintln!("☾ run {id}: creating a sandbox from {snapshot}…");
    let labels = BTreeMap::from([("app".to_string(), "rusty".to_string()), ("rusty-run".to_string(), id.clone())]);
    // A run may be followed from nowhere; don't stop it for being quiet.
    let sandbox = d.create_sandbox(&snapshot, &vars, &labels, 0, Duration::from_secs(300), &mut |sandbox_id| {
        state.sandbox = Some(sandbox_id.into());
        runs.save(&state)
    })?;
    let remote = d.remote(&sandbox)?;
    let task: Vec<String> = match (&args.goal, &args.prompt) {
        (Some(goal), _) => vec!["--goal".into(), goal.clone()],
        (None, Some(prompt)) => vec![prompt.clone()],
        (None, None) => unreachable!("clap requires a goal or a prompt"),
    };
    let started =
        launcher::check_endpoint(&remote).and_then(|()| launcher::setup_and_start(&remote, runs, &mut state, &task));
    if let Err(e) = started {
        eprintln!("☾ {e:#}");
        launcher::delete_sandbox(&d, &remote, runs, &mut state)?;
        return Ok(1);
    }
    eprintln!("☾ running: {}", state.command.as_deref().unwrap_or_default());
    catch_interrupt();
    if follow(&remote, runs, &mut state)?.is_none() {
        return Ok(130);
    }
    launcher::finish(&d, &remote, runs, &mut state, args.keep)
}

#[cfg(test)]
mod source_archive_tests {
    use super::*;

    #[test]
    fn source_archive_accepts_trailing_padding_and_cleans_up_after_failure() {
        let root = std::env::temp_dir().join(format!("rusty-source-test-{}", super::super::digest::random_hex(8)));
        crate::privacy::private_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let input = root.join("input");
        let output = root.join("output");
        crate::privacy::private_dir(&input).unwrap();
        crate::privacy::private_dir(&output).unwrap();
        std::fs::write(input.join("tracked.txt"), b"complete source\n").unwrap();
        std::fs::write(input.join(".rusty-source.tar"), b"legitimate tracked content\n").unwrap();
        let tar = std::process::Command::new("tar")
            .arg("cf")
            .arg("-")
            .arg("-C")
            .arg(&input)
            .args([".rusty-source.tar", "tracked.txt"])
            .output()
            .unwrap();
        assert!(tar.status.success());
        let mut archive = tar.stdout;
        // More trailing padding than a pipe can buffer makes the original
        // write-all-before-wait path fail when tar exits at the end markers.
        archive.resize(archive.len() + 2 * 1024 * 1024, 0);
        extract_source_archive(&archive, &output).unwrap();
        assert_eq!(std::fs::read(output.join("tracked.txt")).unwrap(), b"complete source\n");
        assert_eq!(std::fs::read(output.join(".rusty-source.tar")).unwrap(), b"legitimate tracked content\n");
        let mut remaining: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
        remaining.sort();
        assert_eq!(remaining, [std::ffi::OsString::from("input"), std::ffi::OsString::from("output")]);

        assert!(extract_source_archive(b"not a tar archive", &output).is_err());
        assert_eq!(std::fs::read(output.join("tracked.txt")).unwrap(), b"complete source\n");
        assert_eq!(std::fs::read(output.join(".rusty-source.tar")).unwrap(), b"legitimate tracked content\n");
        let mut remaining: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name()).collect();
        remaining.sort();
        assert_eq!(remaining, [std::ffi::OsString::from("input"), std::ffi::OsString::from("output")]);
    }
}
