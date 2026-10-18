//! The infrastructure harness. A strong model can write a kubectl command;
//! it cannot know on its own which cluster that command will hit, keep a
//! copy of what it is about to change, or prove the rollout finished. This
//! module does the parts a model can't: it works out the live target (kube
//! context, cloud account, terraform workspace, git branch), redacts secrets
//! from tool output, understands kubectl, helm and terraform commands well
//! enough to snapshot before and verify after, and keeps an append-only
//! audit log of everything that touched infrastructure.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one probe may take. Probes run in parallel, so this bounds startup.
const PROBE_SECS: u64 = 4;

/// Where the session's commands will land, as far as rusty can tell.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Target {
    pub kube_context: Option<String>,
    pub kube_namespace: Option<String>,
    pub aws_profile: Option<String>,
    pub aws_account: Option<String>,
    pub aws_region: Option<String>,
    pub gcloud_project: Option<String>,
    pub tf_workspace: Option<String>,
    pub tf_backend: Option<String>,
    pub git_branch: Option<String>,
    /// Some name above looks like production.
    pub production: bool,
}

impl Target {
    /// Reads the live targets from local config only: no network and no
    /// slow CLIs, so startup stays instant. `RUSTY_INFRA=off` skips it.
    pub fn detect(cwd: &Path) -> Target {
        Self::probe(cwd, false)
    }

    /// The same, plus the AWS account behind the profile (one STS call).
    /// Used by `/target`, where the user asked for it.
    pub fn detect_with_identity(cwd: &Path) -> Target {
        Self::probe(cwd, true)
    }

    fn probe(cwd: &Path, identity: bool) -> Target {
        if std::env::var("RUSTY_INFRA").is_ok_and(|v| v == "off") {
            return Target::default();
        }
        let kube = std::thread::spawn(probe_kube);
        let aws = std::thread::spawn(move || probe_aws(identity));
        let gcloud = std::thread::spawn(probe_gcloud);
        let mut t = Target { git_branch: probe_git(cwd), ..Target::default() };
        (t.tf_workspace, t.tf_backend) = probe_terraform(cwd);
        if let Ok(Some((ctx, ns))) = kube.join() {
            t.kube_context = Some(ctx);
            t.kube_namespace = Some(ns);
        }
        if let Ok(Some((profile, account, region))) = aws.join() {
            t.aws_profile = Some(profile);
            t.aws_account = account;
            t.aws_region = region;
        }
        t.gcloud_project = gcloud.join().ok().flatten();
        t.production = [&t.kube_context, &t.kube_namespace, &t.aws_profile, &t.gcloud_project, &t.tf_workspace]
            .iter()
            .any(|v| v.as_deref().is_some_and(looks_production));
        t
    }

    pub fn is_empty(&self) -> bool {
        *self == Target::default()
    }

    /// One line: `kube prod-eu/payments · aws ops (123456789012, eu-west-1) · git main`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ctx) = &self.kube_context {
            parts.push(format!("kube {ctx}/{}", self.kube_namespace.as_deref().unwrap_or("default")));
        }
        if let Some(p) = &self.aws_profile {
            let mut extra: Vec<&str> = Vec::new();
            extra.extend(self.aws_account.as_deref());
            extra.extend(self.aws_region.as_deref());
            let extra = if extra.is_empty() { String::new() } else { format!(" ({})", extra.join(", ")) };
            parts.push(format!("aws {p}{extra}"));
        }
        if let Some(p) = &self.gcloud_project {
            parts.push(format!("gcloud {p}"));
        }
        if let Some(w) = &self.tf_workspace {
            let backend = self.tf_backend.as_ref().map(|b| format!(" ({b})")).unwrap_or_default();
            parts.push(format!("terraform {w}{backend}"));
        }
        if let Some(b) = &self.git_branch {
            parts.push(format!("git {b}"));
        }
        parts.join(" · ")
    }

    /// The block the model sees. Short: it is paid for on every step.
    pub fn prompt_block(&self, careful: bool) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut s = format!("\nLive infrastructure targets: {}.", self.summary());
        if self.production {
            s.push_str(" One of these looks like PRODUCTION.");
        }
        s.push_str(
            " Commands run against exactly these unless they pass --context, --profile or similar; name the \
             target before any change, and never change one the user has not confirmed. Diff or plan before \
             you apply (kubectl diff or --dry-run=server, helm diff or --dry-run, terraform plan -out=FILE then \
             apply FILE). Never add --force, --prune or -auto-approve unless the user asked for exactly that. \
             rusty snapshots what a change touches before it runs and tells you the rollback path.",
        );
        if careful {
            s.push_str(
                " Careful mode refuses an apply whose dry run has not run in this session against the same files \
                 and target, and runs it again if a file was edited since.",
            );
        }
        s.push('\n');
        s
    }
}

/// `prod`, `prd` or `live` as a whole word or a `-`/`_`/`/`/`.`/`:` delimited part.
pub fn looks_production(name: &str) -> bool {
    let lower = name.to_lowercase();
    let is_sep = |c: char| !c.is_ascii_alphanumeric();
    ["prod", "prd", "live"].iter().any(|w| {
        lower.match_indices(w).any(|(i, _)| {
            let before = lower[..i].chars().next_back().is_none_or(is_sep);
            let after = lower[i + w.len()..].chars().next().is_none_or(is_sep);
            before && after
        })
    })
}

fn probe_kube() -> Option<(String, String)> {
    // kubectl takes ~100 ms to start; with no kubeconfig it has no context
    // to report, so it is not worth asking.
    if !kubeconfig_exists() {
        return None;
    }
    let out = run(
        "kubectl",
        &["config", "view", "--minify", "-o", "jsonpath={.current-context}|{.contexts[0].context.namespace}"],
    )?;
    let (ctx, ns) = out.trim().split_once('|')?;
    if ctx.is_empty() {
        return None;
    }
    Some((ctx.to_string(), if ns.is_empty() { "default".into() } else { ns.to_string() }))
}

/// Any of the files kubectl would read: `$KUBECONFIG`, else ~/.kube/config.
fn kubeconfig_exists() -> bool {
    match std::env::var_os("KUBECONFIG").filter(|v| !v.is_empty()) {
        Some(list) => std::env::split_paths(&list).any(|p| p.exists()),
        None => std::env::var_os("HOME").is_some_and(|h| Path::new(&h).join(".kube/config").exists()),
    }
}

/// (profile, account, region). The region comes from the environment or
/// ~/.aws/config; the account needs one STS call, so only with `identity`.
fn probe_aws(identity: bool) -> Option<(String, Option<String>, Option<String>)> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
    let has_files = home.join(".aws/credentials").exists() || home.join(".aws/config").exists();
    let profile = match env("AWS_PROFILE").or_else(|| env("AWS_DEFAULT_PROFILE")) {
        Some(p) => p,
        None if env("AWS_ACCESS_KEY_ID").is_some() => "env".into(),
        None if has_files => "default".into(),
        None => return None,
    };
    let config = env("AWS_CONFIG_FILE").map(PathBuf::from).unwrap_or_else(|| home.join(".aws/config"));
    let section = if matches!(profile.as_str(), "default" | "env") {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let region = env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| ini_value(&std::fs::read_to_string(config).unwrap_or_default(), &section, "region"));
    let account = identity
        .then(|| run("aws", &["sts", "get-caller-identity", "--output", "text", "--query", "Account"]))
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some((profile, account, region))
}

/// `key = value` from `[section]` of an INI file.
fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name.trim() == section;
        } else if inside {
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == key && !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
    }
    None
}

fn probe_gcloud() -> Option<String> {
    if let Ok(p) = std::env::var("CLOUDSDK_CORE_PROJECT") {
        if !p.trim().is_empty() {
            return Some(p.trim().to_string());
        }
    }
    // gcloud itself takes a second to start; its config files don't.
    let dir = std::env::var("CLOUDSDK_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/gcloud"));
    let active = std::fs::read_to_string(dir.join("active_config")).unwrap_or_else(|_| "default".into());
    let text = std::fs::read_to_string(dir.join(format!("configurations/config_{}", active.trim()))).ok()?;
    ini_value(&text, "core", "project")
}

/// (workspace, backend) from the files terraform leaves behind; no CLI needed.
fn probe_terraform(cwd: &Path) -> (Option<String>, Option<String>) {
    let has_tf = std::fs::read_dir(cwd)
        .map(|rd| rd.filter_map(|e| e.ok()).any(|e| e.path().extension().is_some_and(|x| x == "tf")))
        .unwrap_or(false);
    let dot = cwd.join(".terraform");
    if !has_tf && !dot.exists() {
        return (None, None);
    }
    let workspace = std::env::var("TF_WORKSPACE")
        .ok()
        .or_else(|| std::fs::read_to_string(dot.join("environment")).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".into());
    let backend = std::fs::read_to_string(dot.join("terraform.tfstate"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            let b = &v["backend"];
            let kind = b["type"].as_str()?.to_string();
            let name = b["config"]["bucket"].as_str().or_else(|| b["config"]["path"].as_str());
            Some(match name {
                Some(n) => format!("{kind} {n}"),
                None => kind,
            })
        });
    (Some(workspace), backend)
}

fn probe_git(cwd: &Path) -> Option<String> {
    let out = Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(cwd).output().ok()?;
    let b = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !b.is_empty()).then_some(b)
}

/// Runs a probe with a time limit and returns its stdout on success.
fn run(bin: &str, args: &[&str]) -> Option<String> {
    let mut child =
        Command::new(bin).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (done, output) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        let _ = done.send(buf);
    });
    let deadline = Instant::now() + Duration::from_secs(PROBE_SECS);
    // Output ends when the probe exits (or closes stdout), so wake then
    // rather than on a polling tick.
    let out = output.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return out,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if out.is_none() || Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}

pub use rusty::privacy::{redact, redaction_note};

// -------------------------------------------------------------- commands

/// What a shell command does to infrastructure, as far as the harness needs
/// to know: which tool, whether it changes anything, and whether it is the
/// dry run of a change.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Action {
    pub tool: String,
    pub verb: String,
    pub mutating: bool,
    pub dry_run: bool,
    /// Shared by a change and its dry run; empty when no dry run exists for it.
    pub signature: String,
    /// Local files the dry run depends on; editing one makes it stale.
    pub files: Vec<String>,
    /// (file name, read-only command) pairs captured before the change runs.
    pub snapshots: Vec<(String, String)>,
    /// How to undo the change from the snapshot, for the model and the record.
    pub rollback: String,
    /// The dry run careful mode wants to have seen first, if one exists.
    pub dry_run_hint: String,
    /// How to prove the change landed healthy, run after it in careful mode.
    pub verify: Verify,
}

/// A check that a change finished: rollouts for Kubernetes workloads, the
/// release status for helm, an empty plan for terraform.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Verify {
    #[default]
    None,
    Kube {
        /// A `get ... -o name` that lists the objects the change touched.
        list: Option<String>,
        /// Workloads named on the command line.
        objects: Vec<String>,
        flags: String,
    },
    Helm {
        release: String,
        flags: String,
    },
    Terraform {
        chdir_flag: String,
    },
}

/// Tools whose commands are worth an audit line even when rusty does not
/// understand them in detail.
const INFRA_CLIS: &[&str] = &[
    "aws",
    "gcloud",
    "az",
    "docker",
    "docker-compose",
    "ssh",
    "ansible",
    "ansible-playbook",
    "flux",
    "argocd",
    "pulumi",
    "eksctl",
    "istioctl",
    "velero",
    "vault",
    "consul",
    "nomad",
    "psql",
    "mysql",
    "redis-cli",
    "fly",
    "flyctl",
    "doctl",
    "heroku",
    "vercel",
    "wrangler",
    "cdk",
    "sam",
    "serverless",
    "kustomize",
    "oc",
];

/// Looks at every segment of a command line and returns the first one that
/// touches infrastructure.
pub fn inspect(cmd: &str) -> Option<Action> {
    segments(cmd).iter().find_map(|seg| {
        let ws = words(seg);
        let ws: Vec<&str> =
            ws.iter().map(String::as_str).skip_while(|w| w.contains('=') && !w.starts_with('-')).collect();
        let (first, rest) = ws.split_first()?;
        let bin = first.rsplit('/').next().unwrap_or(first);
        match bin {
            "kubectl" | "oc" => Some(kubectl_action(bin, rest, seg)),
            "helm" => Some(helm_action(rest, seg)),
            "terraform" | "tofu" => Some(terraform_action(bin, rest)),
            _ if INFRA_CLIS.contains(&bin) => Some(generic_action(bin, rest)),
            _ => None,
        }
    })
}

/// Flags that take a value as the next word.
const KUBECTL_VALUE_FLAGS: &[&str] = &[
    "-n",
    "--namespace",
    "--context",
    "--kubeconfig",
    "--cluster",
    "--user",
    "--as",
    "--as-group",
    "-f",
    "--filename",
    "-k",
    "--kustomize",
    "-l",
    "--selector",
    "-o",
    "--output",
    "-p",
    "--patch",
    "--type",
    "--timeout",
    "--image",
    "--replicas",
    "--field-selector",
    "--grace-period",
    "--template",
    "--from-file",
    "--from-literal",
    "--from-env-file",
    "--port",
    "--target-port",
    "--container",
    "-c",
    "--server",
    "-s",
    "--token",
    "--request-timeout",
    "--min",
    "--max",
    "--cpu-percent",
    "--sort-by",
    "--since",
    "--tail",
    "--revision",
    "--to-revision",
    "--current-replicas",
    "--resource-version",
    "--label-columns",
    "-L",
    "--chunk-size",
];

/// A parsed command line: positionals, the flags the harness reuses, and
/// the ones that change its meaning.
#[derive(Default)]
struct Parsed {
    positionals: Vec<String>,
    files: Vec<String>,
    kustomize: Vec<String>,
    namespace: Option<String>,
    context: Option<String>,
    kubeconfig: Option<String>,
    selector: Option<String>,
    values: Vec<String>,
    dry_run: bool,
    all: bool,
}

fn parse(args: &[&str], value_flags: &[&str]) -> Parsed {
    let mut p = Parsed::default();
    let mut i = 0;
    while i < args.len() {
        let w = args[i];
        let (flag, value) = match w.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f, Some(v.to_string())),
            _ if w.starts_with('-') && value_flags.contains(&w) => {
                i += 1;
                (w, args.get(i).map(|v| v.to_string()))
            }
            _ if w.starts_with('-') => (w, None),
            _ => {
                p.positionals.push(w.to_string());
                i += 1;
                continue;
            }
        };
        match (flag, value) {
            ("-n" | "--namespace", v) => p.namespace = v,
            ("--context" | "--kube-context", v) => p.context = v,
            ("--kubeconfig", v) => p.kubeconfig = v,
            ("-l" | "--selector", v) => p.selector = v,
            ("-f" | "--filename", Some(v)) => p.files.push(v),
            ("-k" | "--kustomize", Some(v)) => p.kustomize.push(v),
            ("--values", Some(v)) => p.values.push(v),
            ("--dry-run", v) => p.dry_run = v.as_deref() != Some("none"),
            ("--all" | "-A" | "--all-namespaces", _) => p.all = true,
            _ => {}
        }
        i += 1;
    }
    p
}

/// The context/namespace flags, for commands the harness runs itself.
fn kube_flags(p: &Parsed) -> String {
    let mut s = String::new();
    for (flag, v) in [("-n", &p.namespace), ("--context", &p.context), ("--kubeconfig", &p.kubeconfig)] {
        if let Some(v) = v {
            s.push_str(&format!(" {flag} {}", quote(v)));
        }
    }
    s
}

const KUBECTL_MUTATING: &[&str] = &[
    "apply",
    "create",
    "replace",
    "patch",
    "edit",
    "scale",
    "set",
    "rollout",
    "delete",
    "label",
    "annotate",
    "taint",
    "cordon",
    "uncordon",
    "drain",
    "expose",
    "run",
    "autoscale",
    "exec",
    "cp",
];

fn kubectl_action(bin: &str, args: &[&str], seg: &str) -> Action {
    let p = parse(args, KUBECTL_VALUE_FLAGS);
    let verb = p.positionals.first().cloned().unwrap_or_default();
    let mut files: Vec<String> = p.files.iter().chain(&p.kustomize).cloned().collect();
    files.sort();
    let signature = if files.is_empty() || files.iter().any(|f| f == "-") {
        String::new()
    } else {
        format!("{bin}{} {}", kube_flags(&p), files.join(" "))
    };
    let dry_run = verb == "diff" || (p.dry_run && matches!(verb.as_str(), "apply" | "create" | "replace"));
    let mutating = !dry_run && KUBECTL_MUTATING.contains(&verb.as_str());
    let sub = if matches!(verb.as_str(), "rollout" | "set") {
        p.positionals.get(1).cloned().unwrap_or_default()
    } else {
        String::new()
    };
    let verb = if sub.is_empty() { verb } else { format!("{verb} {sub}") };
    // rollout status/history and set with no subject are reads.
    let mutating = mutating && !matches!(verb.as_str(), "rollout status" | "rollout history");
    let flags = kube_flags(&p);
    let mut snapshots = Vec::new();
    let mut rollback = String::new();
    let mut verify = Verify::None;
    let skip = if sub.is_empty() { 1 } else { 2 };
    if mutating && !matches!(verb.as_str(), "exec" | "cp" | "run" | "expose" | "autoscale") {
        let subject = kube_subject(&p, skip);
        if !subject.is_empty() {
            snapshots.push(("before.yaml".into(), format!("{bin} get {subject} -o yaml --ignore-not-found{flags}")));
            rollback = format!("{bin} apply -f SNAPSHOT/rollback.yaml{flags}");
        }
        // Deletes have nothing to roll out; everything else waits for its workloads.
        if verb != "delete" && !matches!(verb.as_str(), "rollout pause" | "cordon" | "uncordon" | "drain" | "taint") {
            let list =
                (!p.files.is_empty() || !p.kustomize.is_empty()).then(|| format!("{bin} get {subject} -o name{flags}"));
            let objects = if list.is_some() { Vec::new() } else { kube_workloads(&p, skip) };
            if list.is_some() || !objects.is_empty() {
                verify = Verify::Kube { list, objects, flags: flags.clone() };
            }
        }
    }
    // Applying files is the one kubectl change with a real dry run.
    let dry_run_hint = match verb.as_str() {
        "apply" | "create" | "replace" if mutating && !p.files.is_empty() && p.files.iter().all(|f| f != "-") => {
            format!("{} --dry-run=server", seg.trim())
        }
        "apply" | "create" | "replace" if mutating && !p.kustomize.is_empty() => {
            format!("{} --dry-run=server", seg.trim())
        }
        "apply" | "create" | "replace" if mutating => {
            "write the manifest to a file and diff that file; careful mode does not apply from stdin".into()
        }
        _ => String::new(),
    };
    Action { tool: bin.into(), verb, mutating, dry_run, signature, files, snapshots, rollback, dry_run_hint, verify }
}

/// The workloads a command names, as `TYPE/NAME`, from `TYPE/NAME` or
/// `TYPE NAME...` after the verb.
fn kube_workloads(p: &Parsed, skip: usize) -> Vec<String> {
    let resources: Vec<&String> = p.positionals.iter().skip(skip).take_while(|w| !w.contains('=')).collect();
    let objects: Vec<String> = match resources.split_first() {
        Some((first, rest)) if first.contains('/') => {
            resources.iter().map(|r| r.to_string()).filter(|_| rest.len() < usize::MAX).collect()
        }
        Some((kind, names)) if !names.is_empty() => names.iter().map(|n| format!("{kind}/{n}")).collect(),
        _ => Vec::new(),
    };
    objects.into_iter().filter(|o| is_workload(o)).collect()
}

/// `deployment.apps/api`, `deploy/api`, `sts/db`, `daemonset/node-exporter`.
fn is_workload(object: &str) -> bool {
    let kind = object.split('/').next().unwrap_or("").split('.').next().unwrap_or("").to_ascii_lowercase();
    matches!(
        kind.as_str(),
        "deploy"
            | "deployment"
            | "deployments"
            | "sts"
            | "statefulset"
            | "statefulsets"
            | "ds"
            | "daemonset"
            | "daemonsets"
    )
}

/// What a kubectl change is about, as `get` arguments: the files it applies,
/// or the resources named after the verb. Empty when there is nothing to
/// fetch (stdin, or a brand-new object).
fn kube_subject(p: &Parsed, skip: usize) -> String {
    if p.files.iter().any(|f| f == "-") {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for f in &p.files {
        parts.push(format!("-f {}", quote(f)));
    }
    for k in &p.kustomize {
        parts.push(format!("-k {}", quote(k)));
    }
    if parts.is_empty() {
        // `TYPE/NAME`, `TYPE NAME...` or `TYPE -l app=x`; `k=v` arguments end the list.
        let resources: Vec<&String> = p.positionals.iter().skip(skip).take_while(|w| !w.contains('=')).collect();
        if resources.is_empty() {
            return String::new();
        }
        let verb = p.positionals.first().map(String::as_str).unwrap_or("");
        if matches!(verb, "cordon" | "uncordon" | "drain") {
            parts.push("node".into());
        }
        parts.extend(resources.iter().map(|r| quote(r)));
        if let Some(sel) = &p.selector {
            parts.push(format!("-l {}", quote(sel)));
        }
    }
    parts.join(" ")
}

const HELM_VALUE_FLAGS: &[&str] = &[
    "-n",
    "--namespace",
    "--kube-context",
    "--kubeconfig",
    "-f",
    "--values",
    "--set",
    "--set-string",
    "--set-file",
    "--set-json",
    "--version",
    "--timeout",
    "--description",
    "--repo",
    "--username",
    "--password",
    "--ca-file",
    "--cert-file",
    "--key-file",
    "--history-max",
    "-o",
    "--output",
    "--post-renderer",
    "--revision",
];

fn helm_action(args: &[&str], seg: &str) -> Action {
    let p = parse(args, HELM_VALUE_FLAGS);
    let verb = p.positionals.first().cloned().unwrap_or_default();
    // `helm diff upgrade R C` and `helm template R C` line up with `helm upgrade R C`.
    let offset = if verb == "diff" { 2 } else { 1 };
    let release = p.positionals.get(offset).cloned().unwrap_or_default();
    let chart = p.positionals.get(offset + 1).cloned().unwrap_or_default();
    // helm takes values with -f, which parse() files under `files`.
    let mut files: Vec<String> = p.files.iter().chain(&p.values).cloned().collect();
    if chart.starts_with('.') || chart.starts_with('/') {
        files.push(chart.clone());
    }
    files.sort();
    let dry_run = verb == "diff" || verb == "template" || (p.dry_run && matches!(verb.as_str(), "upgrade" | "install"));
    let mutating = !dry_run && matches!(verb.as_str(), "upgrade" | "install" | "rollback" | "uninstall" | "delete");
    let flags = kube_flags(&p);
    let signature = if release.is_empty() || (chart.is_empty() && verb != "rollback") {
        String::new()
    } else {
        format!("helm{flags} {release} {chart} {}", files.join(" "))
    };
    let mut snapshots = Vec::new();
    let mut rollback = String::new();
    if mutating && !release.is_empty() {
        let r = quote(&release);
        snapshots.push(("values.yaml".into(), format!("helm get values {r} -o yaml{flags}")));
        snapshots.push(("manifest.yaml".into(), format!("helm get manifest {r}{flags}")));
        rollback = format!("helm rollback {r}{flags} (previous values in SNAPSHOT/values.yaml)");
    }
    let dry_run_hint = if mutating && matches!(verb.as_str(), "upgrade" | "install") {
        format!("{} --dry-run (or helm diff upgrade … if the plugin is installed)", seg.trim())
    } else {
        String::new()
    };
    let verify = if mutating && !release.is_empty() && matches!(verb.as_str(), "upgrade" | "install" | "rollback") {
        Verify::Helm { release: quote(&release), flags: flags.clone() }
    } else {
        Verify::None
    };
    Action { tool: "helm".into(), verb, mutating, dry_run, signature, files, snapshots, rollback, dry_run_hint, verify }
}

const TERRAFORM_VALUE_FLAGS: &[&str] =
    &["-out", "-var", "-var-file", "-target", "-replace", "-state", "-backend-config", "-lock-timeout", "-parallelism"];

fn terraform_action(bin: &str, args: &[&str]) -> Action {
    let chdir = args.iter().find_map(|a| a.strip_prefix("-chdir=")).unwrap_or("").to_string();
    let p = parse(args, TERRAFORM_VALUE_FLAGS);
    let verb = p.positionals.first().cloned().unwrap_or_default();
    let out = args
        .iter()
        .position(|a| *a == "-out")
        .and_then(|i| args.get(i + 1).copied())
        .or_else(|| args.iter().find_map(|a| a.strip_prefix("-out=")));
    let plan_file = match verb.as_str() {
        "plan" => out.map(str::to_string),
        "apply" => p.positionals.get(1).cloned(),
        _ => None,
    };
    let signature = plan_file.as_ref().map(|f| format!("{bin} {chdir} {f}")).unwrap_or_default();
    let dry_run = matches!(verb.as_str(), "plan" | "validate" | "show" | "fmt" | "console" | "graph" | "output");
    let mutating = matches!(verb.as_str(), "apply" | "destroy" | "import" | "taint" | "untaint" | "force-unlock")
        || (verb == "state"
            && p.positionals.get(1).is_some_and(|s| matches!(s.as_str(), "mv" | "rm" | "push" | "replace-provider")))
        || (verb == "workspace" && p.positionals.get(1).is_some_and(|s| s == "delete"));
    let verb = match p.positionals.get(1) {
        Some(sub) if matches!(verb.as_str(), "state" | "workspace") => format!("{verb} {sub}"),
        _ => verb,
    };
    let mut snapshots = Vec::new();
    let mut rollback = String::new();
    let chdir_flag = if chdir.is_empty() { String::new() } else { format!(" -chdir={}", quote(&chdir)) };
    if mutating {
        snapshots.push(("terraform.tfstate".into(), format!("{bin}{chdir_flag} state pull")));
        rollback = "re-apply the previous code revision; SNAPSHOT/terraform.tfstate is the state before the change \
                    (terraform state push restores state only, not the real resources)"
            .into();
    }
    let dry_run_hint = match verb.as_str() {
        "apply" if plan_file.is_some() => format!("{bin}{chdir_flag} plan -out={}", plan_file.as_deref().unwrap_or("")),
        "apply" => {
            format!("{bin}{chdir_flag} plan -out=rusty.tfplan, read it, then {bin}{chdir_flag} apply rusty.tfplan")
        }
        "destroy" => format!(
            "{bin}{chdir_flag} plan -destroy -out=rusty.tfplan, read it, then {bin}{chdir_flag} apply rusty.tfplan"
        ),
        _ => String::new(),
    };
    let verify =
        if verb == "apply" { Verify::Terraform { chdir_flag: format!("{bin}{chdir_flag}") } } else { Verify::None };
    Action {
        tool: bin.into(),
        verb,
        mutating,
        dry_run,
        signature,
        files: plan_file.into_iter().collect(),
        snapshots,
        rollback,
        dry_run_hint,
        verify,
    }
}

/// Other infrastructure CLIs: logged, with a verb-based guess at whether
/// they change anything. Permissions, not this guess, decide what runs.
fn generic_action(bin: &str, args: &[&str]) -> Action {
    let verbs: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with('-')).take(3).collect();
    let read = verbs.iter().any(|v| {
        [
            "describe", "get", "list", "ls", "show", "status", "logs", "ps", "images", "inspect", "version", "info",
            "diff", "preview", "events", "top", "search", "filter", "lookup", "query", "scan", "head", "cat", "stat",
            "whoami", "check",
        ]
        .iter()
        .any(|r| v == r || v.starts_with(&format!("{r}-")))
    });
    Action { tool: bin.into(), verb: verbs.join(" "), mutating: !read && !verbs.is_empty(), ..Action::default() }
}

/// Splits on `&&`, `||`, `;`, `|` and newlines that are outside quotes.
fn segments(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for ch in cmd.chars() {
        if escaped {
            escaped = false;
            cur.push(ch);
            continue;
        }
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\\') => escaped = true,
            (None, '"' | '\'') => quote = Some(ch),
            (None, ';' | '|' | '&' | '\n') => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Whitespace-separated words with quotes removed and quoted spans kept whole.
fn words(seg: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut quoted = false;
    for ch in seg.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                quoted = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || quoted {
                    out.push(std::mem::take(&mut cur));
                    quoted = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || quoted {
        out.push(cur);
    }
    out
}

/// Shell-quotes a word for commands the harness builds itself.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@' | '=' | ','))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

// --------------------------------------------------------------- harness

/// What happened to one infrastructure command. The audit log on disk gets
/// every one; the change record at the end of the session lists the ones
/// that changed something.
#[derive(Debug, Clone)]
pub struct Record {
    pub t: u64,
    pub command: String,
    pub action: Action,
    /// `ran`, `blocked`, `denied`, `declined`, `timed out` or `interrupted`.
    pub outcome: String,
    pub exit: Option<i32>,
    pub secs: f32,
    pub snapshot: Option<String>,
    pub verified: Option<bool>,
    pub note: String,
}

/// Per-session harness state: the target, where the audit log lives, the
/// dry runs seen so far and this session's records.
#[derive(Default)]
pub struct Harness {
    pub target: Target,
    /// The project's rusty directory; None means nothing is written.
    pub dir: Option<PathBuf>,
    pub records: Vec<Record>,
    /// Dry runs that have run this session: (signature, files they depend on).
    dry_runs: Vec<(String, Vec<String>)>,
}

impl Harness {
    /// Remembers a dry run that completed, so the matching change may run.
    pub fn saw_dry_run(&mut self, action: &Action, exit: Option<i32>) {
        let completed = match action.tool.as_str() {
            // diff exits 1 when there are differences; plan exits 2 with -detailed-exitcode.
            "kubectl" | "oc" => matches!(exit, Some(0 | 1)),
            "terraform" | "tofu" => matches!(exit, Some(0 | 2)),
            _ => exit == Some(0),
        };
        if action.dry_run && completed && !action.signature.is_empty() {
            self.dry_runs.retain(|(sig, _)| *sig != action.signature);
            self.dry_runs.push((action.signature.clone(), action.files.clone()));
        }
    }

    /// Editing a file a dry run depended on makes that dry run stale.
    pub fn invalidate(&mut self, path: &str) {
        let p = path.trim_start_matches("./");
        self.dry_runs.retain(|(_, files)| {
            !files.iter().any(|f| {
                let f = f.trim_start_matches("./").trim_end_matches('/');
                p == f || p.starts_with(&format!("{f}/"))
            })
        });
    }

    /// In careful mode a change that has a dry run must have run it first,
    /// in this session, against the same files and target. Returns the
    /// refusal to hand the model, or None to let the change run.
    pub fn gate(&self, action: &Action) -> Option<String> {
        if !action.mutating || action.dry_run_hint.is_empty() {
            return None;
        }
        if !action.signature.is_empty() && self.dry_runs.iter().any(|(sig, _)| *sig == action.signature) {
            return None;
        }
        let why = if action.signature.is_empty() {
            "it has no reviewable dry run".to_string()
        } else if self.records.iter().any(|r| r.action.dry_run && r.action.signature == action.signature) {
            "its dry run is stale: a file it depends on was edited since".to_string()
        } else {
            "no diff or plan for it has run in this session".to_string()
        };
        Some(format!(
            "blocked by careful mode: {why}. Run `{}` first, read the result, then run the change again. This is \
             the harness, not a permission: it cannot be approved, and piping or splitting the command does not \
             change it.",
            action.dry_run_hint
        ))
    }

    pub fn audit_path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join("audit.jsonl"))
    }

    /// Logs one command: a line in the audit file and an entry in memory.
    pub fn record(&mut self, mode: &str, mut rec: Record) {
        rec.t = rusty::unix_now();
        let line = json!({
            "t": rec.t, "mode": mode, "target": self.target.summary(), "command": rec.command,
            "tool": rec.action.tool, "verb": rec.action.verb, "mutating": rec.action.mutating, "dry_run": rec.action.dry_run,
            "outcome": rec.outcome, "exit": rec.exit, "secs": rec.secs, "snapshot": rec.snapshot,
            "verified": rec.verified, "note": rec.note,
        });
        if let Some(path) = self.audit_path() {
            append_private(&path, &format!("{line}\n"));
        }
        self.records.push(rec);
    }

    /// Captures the current state of what a change is about to touch, into a
    /// fresh directory only the owner can read. Returns the directory, or a
    /// note saying why there is nothing to keep.
    pub fn snapshot(&self, action: &Action, command: &str) -> Result<PathBuf, String> {
        self.snapshot_with(action, command, crate::tools::run)
    }

    pub fn snapshot_with(
        &self,
        action: &Action,
        command: &str,
        run: impl Fn(&str, Duration) -> anyhow::Result<(Option<Option<i32>>, String, String)>,
    ) -> Result<PathBuf, String> {
        if action.snapshots.is_empty() {
            return Err("nothing to snapshot for this command".into());
        }
        let Some(dir) = &self.dir else { return Err("no project directory to keep snapshots in".into()) };
        let slug: String = format!("{}-{}", action.tool, action.verb).replace(' ', "-");
        let path = dir.join("snapshots").join(format!("{}-{slug}", rusty::unix_now()));
        let mut kept = 0;
        let mut errors = Vec::new();
        let mut files = Vec::new();
        for (name, cmd) in &action.snapshots {
            match run(cmd, Duration::from_secs(SNAPSHOT_SECS)) {
                Ok((Some(Some(0)), out, _)) if !out.trim().is_empty() => {
                    if redact(&out).1 > 0 {
                        return Err("snapshot contains secret values; no snapshot was persisted".into());
                    }
                    files.push((name.clone(), out));
                    kept += 1;
                }
                Ok((Some(Some(0)), _, _)) => {}
                Ok((_, _, err)) => errors.push(format!("`{cmd}`: {}", err.trim().lines().last().unwrap_or("failed"))),
                Err(e) => errors.push(format!("`{cmd}`: {e}")),
            }
        }
        if kept == 0 {
            return Err(if errors.is_empty() {
                "nothing to snapshot: the objects don't exist yet".into()
            } else {
                format!("snapshot failed: {}", errors.join("; "))
            });
        }
        create_private_dir(&path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        for (name, out) in &files {
            write_private(&path.join(name), out);
            if name == "before.yaml" {
                write_private(&path.join("rollback.yaml"), &strip_volatile(out));
            }
        }
        write_private(&path.join("command.txt"), &format!("{command}\n"));
        Ok(path)
    }

    /// Proves a change landed: waits for every workload it touched to roll
    /// out, checks the helm release status, or runs a post-apply plan that
    /// must be empty. Returns (healthy, what was checked).
    pub fn verify(&self, action: &Action) -> (bool, String) {
        self.verify_with(action, crate::tools::run)
    }

    pub fn verify_with(
        &self,
        action: &Action,
        execute: impl Fn(&str, Duration) -> anyhow::Result<(Option<Option<i32>>, String, String)>,
    ) -> (bool, String) {
        let timeout = rollout_timeout();
        let limit = Duration::from_secs(timeout + 30);
        let run = |cmd: &str| match execute(cmd, limit) {
            Ok((Some(Some(code)), out, err)) => (Some(code), format!("{out}{err}")),
            Ok((_, out, err)) => (None, format!("{out}{err}timed out after {}s", limit.as_secs())),
            Err(e) => (None, e.to_string()),
        };
        let tail = |text: &str| text.trim().lines().last().unwrap_or("").trim().to_string();
        match &action.verify {
            Verify::None => (true, "nothing to verify for this command".into()),
            Verify::Kube { list, objects, flags } => {
                let mut objects = objects.clone();
                let mut notes = Vec::new();
                if let Some(list) = list {
                    let (code, out) = run(list);
                    if code != Some(0) {
                        return (false, format!("could not list the objects: {}", tail(&out)));
                    }
                    let all: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
                    objects.extend(all.iter().filter(|o| is_workload(o)).map(|o| o.to_string()));
                    if objects.is_empty() {
                        return (true, format!("{} present, no workloads to roll out", all.join(", ")));
                    }
                }
                let tool = action.tool.as_str();
                let mut healthy = true;
                for o in &objects {
                    let (code, out) = run(&format!("{tool} rollout status {o} --timeout={timeout}s{flags}"));
                    healthy &= code == Some(0);
                    notes.push(format!("{o}: {}", tail(&out)));
                }
                (healthy, notes.join("; "))
            }
            Verify::Helm { release, flags } => {
                let (code, out) = run(&format!("helm status {release}{flags}"));
                let status = out.lines().find_map(|l| l.strip_prefix("STATUS: ")).unwrap_or("").trim().to_string();
                if code != Some(0) || status != "deployed" {
                    return (
                        false,
                        format!("helm status {release}: {}", if status.is_empty() { tail(&out) } else { status }),
                    );
                }
                let (code, out) =
                    run(&format!("helm get manifest {release}{flags} | {} get -f - -o name{flags}", "kubectl"));
                let workloads: Vec<String> =
                    out.lines().map(str::trim).filter(|o| is_workload(o)).map(String::from).collect();
                if code != Some(0) || workloads.is_empty() {
                    return (true, format!("release {release} deployed; no workloads to roll out"));
                }
                let mut healthy = true;
                let mut notes = vec![format!("release {release} deployed")];
                for o in &workloads {
                    let (code, out) = run(&format!("kubectl rollout status {o} --timeout={timeout}s{flags}"));
                    healthy &= code == Some(0);
                    notes.push(format!("{o}: {}", tail(&out)));
                }
                (healthy, notes.join("; "))
            }
            Verify::Terraform { chdir_flag } => {
                let (code, out) = run(&format!("{chdir_flag} plan -detailed-exitcode -input=false -no-color"));
                match code {
                    Some(0) => (true, "post-apply plan is empty: state matches the configuration".into()),
                    Some(2) => (false, "post-apply plan still has changes: the apply did not converge".into()),
                    _ => (false, format!("post-apply plan failed: {}", tail(&out))),
                }
            }
        }
    }

    /// Records a command that never ran, with why.
    pub fn record_refusal(&mut self, mode: &str, cmd: &str, outcome: &str, note: &str) {
        if let Some(action) = inspect(cmd) {
            self.record(
                mode,
                Record { command: cmd.into(), action, outcome: outcome.into(), note: note.into(), ..empty_record() },
            );
        }
    }

    /// This session's changes: what ran against what, with the rollback
    /// path and whether it was verified. Empty when nothing changed.
    pub fn change_record(&self) -> String {
        let changes: Vec<&Record> = self.records.iter().filter(|r| r.action.mutating).collect();
        if changes.is_empty() {
            return String::new();
        }
        let mut s = format!("change record · {} · target {}\n", plural(changes.len(), "change"), self.target.summary());
        for r in changes {
            let mark = match (r.outcome.as_str(), r.exit) {
                ("ran", Some(0)) => "✓",
                ("ran", _) => "✗",
                _ => "⊘",
            };
            let mut tail = Vec::new();
            if r.outcome != "ran" {
                tail.push(r.outcome.clone());
            } else if r.exit != Some(0) {
                tail.push(format!("exit {}", r.exit.map_or("?".into(), |c| c.to_string())));
            }
            if let Some(p) = &r.snapshot {
                tail.push(format!("snapshot {p}"));
            }
            match r.verified {
                Some(true) => tail.push("verified".into()),
                Some(false) => tail.push("NOT verified".into()),
                None if r.outcome == "ran" => tail.push("not verified".into()),
                None => {}
            }
            if !r.note.is_empty() {
                tail.push(r.note.clone());
            }
            s.push_str(&format!(
                "  {} {mark} {}\n      {}\n",
                clock(r.t),
                crate::ui::truncate(r.command.lines().next().unwrap_or(""), 100),
                tail.join(" · ")
            ));
        }
        if let Some(p) = self.audit_path() {
            s.push_str(&format!("  audit log: {}\n", p.display()));
        }
        s
    }

    /// The last `n` lines of the audit log across all sessions, oldest first.
    pub fn audit_tail(&self, n: usize) -> String {
        let Some(path) = self.audit_path() else { return String::new() };
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let mut out = String::new();
        for line in lines.iter().rev().take(n).rev() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let t = v["t"].as_u64().unwrap_or(0);
            let outcome = v["outcome"].as_str().unwrap_or("");
            let mark = match (outcome, v["exit"].as_i64()) {
                ("ran", Some(0)) => "✓",
                ("ran", _) => "✗",
                _ => "⊘",
            };
            let kind = if v["mutating"].as_bool() == Some(true) {
                "change"
            } else if v["dry_run"].as_bool() == Some(true) {
                "dry run"
            } else {
                "read"
            };
            let mut extra = vec![kind.to_string()];
            if outcome != "ran" {
                extra.push(outcome.into());
            }
            if v["snapshot"].is_string() {
                extra.push("snapshot".into());
            }
            if let Some(ok) = v["verified"].as_bool() {
                extra.push(if ok { "verified".into() } else { "NOT verified".into() });
            }
            out.push_str(&format!(
                "  {} {} {mark} {}  {}\n",
                day(t),
                clock(t),
                crate::ui::truncate(v["command"].as_str().unwrap_or("").lines().next().unwrap_or(""), 80),
                extra.join(" · ")
            ));
        }
        out
    }
}

pub fn empty_record() -> Record {
    Record {
        t: 0,
        command: String::new(),
        action: Action::default(),
        outcome: String::new(),
        exit: None,
        secs: 0.0,
        snapshot: None,
        verified: None,
        note: String::new(),
    }
}

/// How a bash tool result ended, from its first line.
pub fn outcome(result: &str) -> (String, Option<i32>) {
    let first = result.lines().next().unwrap_or("");
    if let Some(code) = first.strip_prefix("exit code: ") {
        return ("ran".into(), code.parse().ok());
    }
    if first.starts_with("timed out") {
        return ("timed out".into(), None);
    }
    ("interrupted".into(), None)
}

/// How long one snapshot command may take.
const SNAPSHOT_SECS: u64 = 60;

/// How long to wait for one rollout. `RUSTY_ROLLOUT_TIMEOUT` overrides it.
fn rollout_timeout() -> u64 {
    std::env::var("RUSTY_ROLLOUT_TIMEOUT").ok().and_then(|v| v.trim_end_matches('s').parse().ok()).unwrap_or(180)
}

/// Drops the fields that make `kubectl get -o yaml` output unapplyable:
/// resourceVersion, uid, creationTimestamp, generation, managedFields and
/// status. Text-based on purpose: no YAML parser, and the raw copy is kept
/// next to it anyway.
pub fn strip_volatile(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len());
    let mut skip_deeper_than: Option<usize> = None;
    let mut metadata_indent: Option<usize> = None;
    for line in yaml.lines() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let key = trimmed.trim_start_matches("- ").split(':').next().unwrap_or("");
        let is_key = trimmed.trim_start_matches("- ").contains(':');
        if let Some(d) = skip_deeper_than {
            // kubectl puts list items at the key's own indent.
            if indent > d || trimmed.is_empty() || (indent == d && trimmed.starts_with("- ")) {
                continue;
            }
            skip_deeper_than = None;
        }
        if metadata_indent.is_some_and(|m| indent <= m) {
            metadata_indent = None;
        }
        if is_key && key == "metadata" && !trimmed.starts_with("- ") {
            metadata_indent = Some(indent);
        }
        let direct_child = metadata_indent.is_some_and(|m| indent == m + 2);
        if direct_child && matches!(key, "resourceVersion" | "uid" | "creationTimestamp" | "generation" | "selfLink") {
            continue;
        }
        if (direct_child && key == "managedFields")
            || (is_key && key == "status" && indent <= 2 && !trimmed.starts_with("- "))
        {
            skip_deeper_than = Some(indent);
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    rusty::privacy::private_dir(path)
}

fn write_private(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = rusty::privacy::private_file(path, false) {
        let _ = f.write_all(redact(text).0.as_bytes());
    }
}

/// Appends to a file only its owner can read.
fn append_private(path: &Path, text: &str) {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = rusty::privacy::private_file(path, true) {
        if let Ok(mut value) = serde_json::from_str::<Value>(text) {
            rusty::privacy::scrub(&mut value);
            let _ = writeln!(f, "{value}");
        } else {
            let _ = f.write_all(redact(text).0.as_bytes());
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// `hh:mm` UTC.
pub fn clock(t: u64) -> String {
    format!("{:02}:{:02}", (t % 86_400) / 3_600, (t % 3_600) / 60)
}

/// `mm-dd` UTC.
fn day(t: u64) -> String {
    let days = (t / 86_400) as i64;
    // Civil-from-days (Howard Hinnant), month and day only.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_snapshot_is_refused_instead_of_saving_a_broken_rollback() {
        let dir = std::env::temp_dir().join(format!("rusty-secret-snapshot-{}", std::process::id()));
        let harness = Harness { dir: Some(dir.clone()), ..Harness::default() };
        let action = Action {
            snapshots: vec![("before.yaml".into(), "printf 'password: snapshot-secret-canary'".into())],
            ..Action::default()
        };
        assert!(harness.snapshot(&action, "inspect").unwrap_err().contains("secret values"));
        assert!(!dir.exists());
    }

    #[test]
    fn ini_values_come_from_the_right_section() {
        let text = "[default]\nregion = us-east-1\n\n[profile prod]\nregion=eu-west-1\n";
        assert_eq!(ini_value(text, "default", "region").as_deref(), Some("us-east-1"));
        assert_eq!(ini_value(text, "profile prod", "region").as_deref(), Some("eu-west-1"));
        assert_eq!(ini_value(text, "profile dev", "region"), None);
    }

    #[test]
    fn redacts_keys_tokens_and_blocks() {
        let cases = [
            ("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "AWS_SECRET_ACCESS_KEY=[redacted]"),
            ("export DB_PASSWORD='s3cret-pass'", "export DB_PASSWORD='[redacted]'"),
            ("mysql -u root --password hunter22 app", "mysql -u root --password [redacted] app"),
            (
                "curl -H 'Authorization: Basic YWRtaW46aHVudGVy' https://x",
                "curl -H 'Authorization: Basic [redacted]' https://x",
            ),
            ("password: hunter22", "password: [redacted]"),
            ("  \"client_secret\": \"abcd1234\",", "  \"client_secret\": \"[redacted]\","),
            ("Authorization: Bearer abcdefghijklmnop", "Authorization: Bearer [redacted]"),
            ("aws_access_key_id = AKIAIOSFODNN7EXAMPLE", "aws_access_key_id = [redacted]"),
            ("token ghp_abcdefghijklmnopqrstuvwxyz012345", "token [redacted]"),
            ("NVIDIA_API_KEY=nvapi-abcdefghijklmnopqrstuvwxyz", "NVIDIA_API_KEY=[redacted]"),
            ("postgres://app:pa55w0rd@db.internal:5432/app", "postgres://app:[redacted]@db.internal:5432/app"),
            ("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abcdefghijklmnop", "jwt [redacted]"),
            ("client-key-data: LS0tLS1CRUdJTi==", "client-key-data: [redacted]"),
            ("{\"password\":\"hunter22\"}", "{\"password\":\"[redacted]\"}"),
            ("password:\thunter22", "password:\t[redacted]"),
        ];
        for (input, want) in cases {
            let (got, n) = redact(input);
            assert_eq!(got, want);
            assert_eq!(n, 1, "{input}");
        }
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nABC\n-----END RSA PRIVATE KEY-----\nafter";
        assert_eq!(redact(pem), ("[redacted private key]\nafter".into(), 1));
        let secret = "apiVersion: v1\nkind: Secret\nmetadata:\n  name: db\ndata:\n  username: YWRtaW4=\n  pw: aHVudGVy\ntype: Opaque\n";
        let (got, n) = redact(secret);
        assert_eq!(n, 2, "{got}");
        assert!(got.contains("  username: [redacted]\n  pw: [redacted]\ntype: Opaque\n"), "{got}");
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        for text in [
            "max_tokens: 5",
            "password: ${DB_PASSWORD}",
            "secretName: db-creds",
            "  key: password",
            "secretKeyRef:\n  name: db\n  key: password",
            "token: null",
            "image: registry/app:1.2.3",
            "https://example.com/path",
            "sha256:abcdef0123456789abcdef0123456789",
            "the user said: sk-ip the test",
            "- name: GITHUB_TOKEN\n  valueFrom:",
            "- secret:access",
            "actions: [secret:read, token:create, credentials:rotate]",
            "action=secret:access resource=srn:simcloud:shop:prod:secret/payments-signing",
            "Authenticate with `Authorization: Bearer <api key>`.",
        ] {
            let (got, n) = redact(text);
            assert_eq!(n, 0, "{text} -> {got}");
            assert_eq!(got, text);
        }
    }

    fn act(cmd: &str) -> Action {
        inspect(cmd).unwrap_or_else(|| panic!("{cmd} is not an infra command"))
    }

    #[test]
    fn kubectl_commands_are_understood() {
        let a = act("kubectl apply -f deploy.yaml -f svc.yaml -n payments --context prod-eu");
        assert!(a.mutating && !a.dry_run);
        assert_eq!(a.signature, "kubectl -n payments --context prod-eu deploy.yaml svc.yaml");
        assert_eq!(a.files, ["deploy.yaml", "svc.yaml"]);
        let d = act("cd k8s && kubectl diff --context prod-eu -f svc.yaml -n payments -f deploy.yaml");
        assert!(d.dry_run && !d.mutating);
        assert_eq!(d.signature, a.signature, "a diff and its apply share a signature");
        let s = act("kubectl apply -f svc.yaml -f deploy.yaml --dry-run=server -n payments --context=prod-eu");
        assert!(s.dry_run && s.signature == a.signature, "{s:?}");
        assert!(act("kubectl apply -f deploy.yaml --dry-run=none").mutating);
        for cmd in [
            "kubectl get pods -n x",
            "kubectl rollout status deploy/api",
            "kubectl logs -f pod/x",
            "kubectl describe node n1",
        ] {
            let a = act(cmd);
            assert!(!a.mutating && !a.dry_run, "{cmd}");
        }
        for cmd in [
            "kubectl scale deploy/api --replicas=3",
            "kubectl set image deployment/api api=img:2",
            "kubectl rollout restart deploy/api",
            "kubectl delete pod x",
            "kubectl patch deploy api -p '{}'",
            "kubectl apply -k overlays/prod",
        ] {
            assert!(act(cmd).mutating, "{cmd}");
        }
        assert_eq!(act("kubectl rollout restart deploy/api").verb, "rollout restart");
        assert!(act("kubectl apply -f -").signature.is_empty(), "stdin has no dry-run signature");
        assert!(inspect("cargo test && ls").is_none());
    }

    #[test]
    fn kubectl_changes_know_what_to_snapshot() {
        let snap = |cmd: &str| act(cmd).snapshots.into_iter().map(|(_, c)| c).collect::<Vec<_>>();
        assert_eq!(
            snap("kubectl apply -f deploy.yaml -f svc.yaml -n payments"),
            ["kubectl get -f deploy.yaml -f svc.yaml -o yaml --ignore-not-found -n payments"]
        );
        assert_eq!(snap("kubectl apply -k overlays/prod"), ["kubectl get -k overlays/prod -o yaml --ignore-not-found"]);
        assert_eq!(
            snap("kubectl scale deploy/api --replicas=3"),
            ["kubectl get deploy/api -o yaml --ignore-not-found"]
        );
        assert_eq!(
            snap("kubectl set image deployment/api api=img:2 --context c"),
            ["kubectl get deployment/api -o yaml --ignore-not-found --context c"]
        );
        assert_eq!(snap("kubectl rollout restart deploy/api"), ["kubectl get deploy/api -o yaml --ignore-not-found"]);
        assert_eq!(snap("kubectl delete pods -l app=api"), ["kubectl get pods -l app=api -o yaml --ignore-not-found"]);
        assert_eq!(
            snap("kubectl patch deploy api -p '{\"a\":1}'"),
            ["kubectl get deploy api -o yaml --ignore-not-found"]
        );
        assert_eq!(
            snap("kubectl drain node-1 --ignore-daemonsets"),
            ["kubectl get node node-1 -o yaml --ignore-not-found"]
        );
        assert!(snap("kubectl apply -f -").is_empty());
        assert!(snap("kubectl run tmp --image=busybox").is_empty());
        assert!(snap("kubectl get pods").is_empty());
        assert_eq!(act("kubectl apply -f d.yaml -n x").rollback, "kubectl apply -f SNAPSHOT/rollback.yaml -n x");
        assert_eq!(
            snap("helm upgrade api ./chart -n payments"),
            ["helm get values api -o yaml -n payments", "helm get manifest api -n payments"]
        );
        assert_eq!(snap("terraform -chdir=infra apply tf.plan"), ["terraform -chdir=infra state pull"]);
        assert_eq!(snap("terraform apply tf.plan"), ["terraform state pull"]);
    }

    #[test]
    fn changes_know_how_to_verify_themselves() {
        let v = |cmd: &str| act(cmd).verify;
        assert_eq!(
            v("kubectl apply -f d.yaml -n payments"),
            Verify::Kube {
                list: Some("kubectl get -f d.yaml -o name -n payments".into()),
                objects: vec![],
                flags: " -n payments".into()
            }
        );
        assert_eq!(
            v("kubectl set image deployment/api api=img:2"),
            Verify::Kube { list: None, objects: vec!["deployment/api".into()], flags: String::new() }
        );
        assert_eq!(
            v("kubectl scale deploy api web --replicas=2"),
            Verify::Kube { list: None, objects: vec!["deploy/api".into(), "deploy/web".into()], flags: String::new() }
        );
        assert!(
            matches!(v("kubectl rollout restart sts/db"), Verify::Kube { ref objects, .. } if objects == &["sts/db"])
        );
        assert_eq!(v("kubectl patch configmap cfg -p '{}'"), Verify::None, "a configmap has no rollout");
        assert_eq!(v("kubectl delete deploy api"), Verify::None);
        assert_eq!(v("kubectl get pods"), Verify::None);
        assert_eq!(v("helm upgrade api ./c -n x"), Verify::Helm { release: "api".into(), flags: " -n x".into() });
        assert_eq!(v("helm uninstall api"), Verify::None);
        assert_eq!(
            v("terraform -chdir=infra apply p.tfplan"),
            Verify::Terraform { chdir_flag: "terraform -chdir=infra".into() }
        );
        assert_eq!(v("terraform import a.b c"), Verify::None);
        assert!(is_workload("deployment.apps/api") && is_workload("ds/x") && !is_workload("service/api"));
    }

    #[test]
    fn stripped_yaml_is_applyable() {
        let yaml = "apiVersion: v1\nitems:\n- apiVersion: apps/v1\n  kind: Deployment\n  metadata:\n    annotations:\n      a: b\n    creationTimestamp: \"2026-01-01T00:00:00Z\"\n    generation: 3\n    managedFields:\n    - apiVersion: apps/v1\n      fieldsType: FieldsV1\n    name: api\n    namespace: payments\n    resourceVersion: \"12345\"\n    uid: abc\n  spec:\n    replicas: 2\n    template:\n      metadata:\n        labels:\n          app: api\n  status:\n    readyReplicas: 2\n    conditions:\n    - type: Available\nkind: List\nmetadata:\n  resourceVersion: \"\"\n";
        let got = strip_volatile(yaml);
        for gone in [
            "resourceVersion",
            "uid:",
            "creationTimestamp",
            "generation",
            "managedFields",
            "fieldsType",
            "status:",
            "readyReplicas",
            "Available",
        ] {
            assert!(!got.contains(gone), "{gone} survived:\n{got}");
        }
        for kept in [
            "kind: Deployment",
            "    name: api",
            "    namespace: payments",
            "      a: b",
            "    replicas: 2",
            "          app: api",
            "kind: List",
        ] {
            assert!(got.contains(kept), "{kept} lost:\n{got}");
        }
    }

    #[test]
    fn helm_and_terraform_commands_are_understood() {
        let up = act("helm upgrade --install api ./chart -n payments -f values.yaml --set image.tag=2");
        assert!(up.mutating);
        assert_eq!(up.signature, "helm -n payments api ./chart ./chart values.yaml");
        let diff = act("helm diff upgrade api ./chart -f values.yaml -n payments");
        assert!(diff.dry_run && diff.signature == up.signature);
        assert!(act("helm upgrade api ./chart --dry-run").dry_run);
        assert!(!act("helm list -A").mutating && !act("helm get values api").mutating);
        assert!(act("helm rollback api 3 -n payments").mutating);
        let plan = act("terraform plan -out=rusty.tfplan -var env=prod");
        assert!(plan.dry_run && plan.signature == "terraform  rusty.tfplan");
        let apply = act("terraform apply rusty.tfplan");
        assert!(apply.mutating && apply.signature == plan.signature);
        assert!(act("terraform apply").signature.is_empty());
        assert!(act("terraform -chdir=infra plan -out tf.plan").signature == "terraform infra tf.plan");
        for cmd in [
            "terraform destroy",
            "terraform state rm aws_instance.x",
            "terraform import a.b id",
            "tofu apply -auto-approve",
        ] {
            assert!(act(cmd).mutating, "{cmd}");
        }
        assert_eq!(act("terraform state rm a.b").verb, "state rm");
        for cmd in ["terraform plan", "terraform state list", "terraform output", "terraform validate"] {
            assert!(!act(cmd).mutating, "{cmd}");
        }
        assert!(!act("aws ec2 describe-instances").mutating);
        assert!(act("aws ec2 terminate-instances --instance-ids i-1").mutating);
        assert!(!act("docker ps").mutating && act("docker rm x").mutating);
    }

    #[test]
    fn careful_gate_wants_a_fresh_dry_run() {
        let mut h = Harness::default();
        let apply = act("kubectl apply -f deploy.yaml -n payments");
        assert!(h.gate(&apply).unwrap().contains("no diff or plan"));
        assert!(h.gate(&act("kubectl scale deploy/api --replicas=2")).is_none(), "scale has no dry run to insist on");
        assert!(h.gate(&act("kubectl get pods")).is_none());
        h.saw_dry_run(&act("kubectl diff -f deploy.yaml -n payments"), Some(1));
        assert!(h.gate(&apply).is_none(), "a diff with differences counts");
        assert!(
            h.gate(&act("kubectl apply -f deploy.yaml -n staging")).is_some(),
            "a different namespace is a different change"
        );
        h.invalidate("./deploy.yaml");
        assert!(h.gate(&apply).is_some(), "editing the file makes the diff stale");
        h.saw_dry_run(&act("kubectl diff -f deploy.yaml -n payments"), Some(2));
        assert!(h.gate(&apply).is_some(), "a diff that errored does not count");
        assert!(h.gate(&act("kubectl apply -f -")).unwrap().contains("stdin"));
        let tf = act("terraform apply rusty.tfplan");
        assert!(h.gate(&tf).is_some());
        assert!(h.gate(&act("terraform apply")).unwrap().contains("plan -out=rusty.tfplan"));
        assert!(h.gate(&act("terraform destroy")).unwrap().contains("plan -destroy"));
        h.saw_dry_run(&act("terraform plan -out=rusty.tfplan"), Some(2));
        assert!(h.gate(&tf).is_none());
        assert!(h.gate(&act("terraform apply other.tfplan")).is_some());
        let helm = act("helm upgrade api ./chart -f values.yaml -n payments");
        assert!(h.gate(&helm).unwrap().contains("--dry-run"));
        h.saw_dry_run(&act("helm diff upgrade api ./chart -f values.yaml -n payments"), Some(0));
        assert!(h.gate(&helm).is_none());
        h.invalidate("chart/templates/deploy.yaml");
        assert!(h.gate(&helm).is_some(), "editing the chart makes the diff stale");
    }

    #[test]
    fn change_record_and_clock() {
        assert_eq!(clock(3_661), "01:01");
        assert_eq!(day(1_759_622_400), "10-05");
        let mut h = Harness::default();
        assert!(h.change_record().is_empty());
        h.record(
            "careful",
            Record {
                command: "kubectl get pods".into(),
                action: act("kubectl get pods"),
                outcome: "ran".into(),
                exit: Some(0),
                ..empty_record()
            },
        );
        assert!(h.change_record().is_empty(), "reads are not changes");
        h.record(
            "careful",
            Record {
                command: "kubectl apply -f d.yaml".into(),
                action: act("kubectl apply -f d.yaml"),
                outcome: "ran".into(),
                exit: Some(0),
                snapshot: Some("/snap/1".into()),
                verified: Some(true),
                ..empty_record()
            },
        );
        h.record_refusal("careful", "terraform apply", "blocked", "no plan file");
        let rec = h.change_record();
        assert!(rec.contains("2 changes"), "{rec}");
        assert!(rec.contains("✓ kubectl apply -f d.yaml") && rec.contains("snapshot /snap/1 · verified"), "{rec}");
        assert!(rec.contains("⊘ terraform apply") && rec.contains("blocked · no plan file"), "{rec}");
    }

    #[test]
    fn production_names() {
        for name in ["prod", "prod-eu", "payments-prod", "arn:aws:eks:eu-west-1:1:cluster/prod-eu", "PRD_2", "live"] {
            assert!(looks_production(name), "{name}");
        }
        for name in ["product-api", "staging", "dev", "oliver", "preprod-x", "reproduce"] {
            assert!(!looks_production(name), "{name}");
        }
    }

    #[test]
    fn summary_and_prompt() {
        let t = Target {
            kube_context: Some("prod-eu".into()),
            kube_namespace: Some("payments".into()),
            aws_profile: Some("ops".into()),
            aws_account: Some("123456789012".into()),
            tf_workspace: Some("prod".into()),
            tf_backend: Some("s3 tf-state".into()),
            git_branch: Some("main".into()),
            production: true,
            ..Target::default()
        };
        assert_eq!(
            t.summary(),
            "kube prod-eu/payments · aws ops (123456789012) · terraform prod (s3 tf-state) · git main"
        );
        assert!(t.prompt_block(false).contains("PRODUCTION"));
        assert!(t.prompt_block(true).contains("refuses an apply") && !t.prompt_block(false).contains("refuses"));
        assert!(Target::default().prompt_block(true).is_empty());
    }

    #[test]
    fn terraform_files_give_workspace_and_backend() {
        let dir = std::env::temp_dir().join(format!("rusty-infra-tf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".terraform")).unwrap();
        assert_eq!(probe_terraform(&dir), (Some("default".into()), None));
        std::fs::write(dir.join(".terraform/environment"), "prod\n").unwrap();
        std::fs::write(
            dir.join(".terraform/terraform.tfstate"),
            r#"{"backend":{"type":"s3","config":{"bucket":"tf-state","key":"app"}}}"#,
        )
        .unwrap();
        assert_eq!(probe_terraform(&dir), (Some("prod".into()), Some("s3 tf-state".into())));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
