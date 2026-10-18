# Running rusty in the cloud

rusty is a single static binary with no runtime dependencies, so it runs
anywhere Linux does. Model keys always travel as environment variables, never
as arguments.

## Daytona

`rusty-cloud` is a second binary in this crate (`cargo build --bins`). It
talks to Daytona's REST API directly with the same HTTP client rusty uses; no
SDK, Python or uv is needed. It reads `DAYTONA_API_KEY` (and optionally
`DAYTONA_TARGET`) from the environment or a private `.env`;
`DAYTONA_API_URL` (default `https://app.daytona.io/api`) is honoured only
from the real environment, so a project's `.env` cannot send your key to
another host.

### Choose what runs where

| Entry point | Reasoning and workers | Memory | Files, search, Bash, infra probes |
|---|---|---|---|
| `rusty --tools local` | local | local | local |
| `rusty-cloud hybrid ...` | local | local | one Daytona workspace |
| `rusty-cloud run ...` | Daytona | Daytona | Daytona |

`--memory off|recall|learn|reflect|deep` on the CLI, or `--memory-mode` on the launcher,
chooses the memory behavior at the agent's location. `deep` may make extra
model calls; `on` uses local hooks without background inference. The three
reasoning modes and permission modes work in each arrangement.

### Local agent, remote swarm tools

Use this when the model endpoint is reachable from your computer but blocked
by sandbox egress policy. NVIDIA requests originate locally. The sandbox
needs access only to the services its code/build tools actually use; this
does not grant it general internet access.

```text
Local Rusty lead + read-only workers ─── model endpoint
             │
      local memory hooks
             │
  authenticated localhost tool bridge (rusty-cloud)
             │
     Daytona toolbox API (/process/execute)
             │
  Rust tool endpoint in one shared checkout
```

Requirements: a local `rusty` and `rusty-cloud` (plus the optional memory
companion), `DAYTONA_API_KEY` and model credentials in your local environment,
and an **already started** sandbox with a project checkout and this build of
the Linux Rusty binary on PATH.

```bash
cargo build --bins
# Configure credentials in your shell or private .env; never paste them into flags.
target/debug/rusty-cloud hybrid \
    --sandbox <existing-id> --workspace /home/daytona/work \
    --local-binary target/debug/rusty \
    --mode standard --agents swarm --swarm-max 3 --memory-mode learn
# Omit --prompt/--goal for the interactive CLI.
# Headless run with a local receipt:
target/debug/rusty-cloud hybrid \
    --sandbox <existing-id> --workspace /home/daytona/work \
    --agents swarm --swarm-max 3 --memory-mode off \
    --prompt "Audit the modules with three read-only workers, then fix and verify." \
    --stats --trajectory hybrid.json
```

If you need a fresh environment, `prepare --source` below builds the current
**committed HEAD**. Create/start a sandbox from the printed snapshot with
Daytona's dashboard or API, then clone the project into it. Those operations
consume cloud credits; `hybrid` performs none of them implicitly. Older Rusty
snapshots lack tool protocol v1 and must be rebuilt. The bridge handshake
checks compatibility and resolves the real remote workspace before any model
request. `DAYTONA_TARGET=eu` selects EU as with the full-cloud launcher.

Placement controls:

- CLI `--tools local|daytona` and `RUSTY_TOOLS` override the saved default.
  `rusty-cloud hybrid` supplies the localhost connection required by `daytona`.
- `/tools` and `/settings` show the current backend and remote workspace.
  `/tools local|daytona` saves the default for the **next** session, without
  moving active work. Missing bridge settings fail clearly, with no local fallback.
- `--agents off|sub|swarm|auto` controls delegation; `--swarm-max 1..8` limits
  concurrency. Hybrid defaults to auto and three workers. Workers share one
  checkout and cannot edit; their model calls stay local. Creating one sandbox
  per worker is deliberately omitted: it adds cost and reconciliation work.
- `--mode` and `--permissions` stay independent. Hybrid defaults to `auto`
  permissions; it does not inherit the full-cloud launcher's `--yolo` flag.

The bridge is a small std::net server bound only to 127.0.0.1 with a random
256-bit per-session token compared in constant time, 1 MiB request/response
bounds, at most nine concurrent RPCs (the lead plus eight workers), 5-second
socket timeouts, no HTTP logs, no redirects, and no retries of uncertain
writes. Transport errors reach rusty only as a generic message, so nothing
from the API (URLs, credentials) is echoed. Each RPC runs `rusty --tool-rpc`
in the sandbox through the toolbox's process API, with the request JSON
shell-quoted as data on stdin and `RUSTY_NO_DOTENV=1`, reusing the same
filesystem/shell implementations. Permission checks
inspect **remote** scripts and branch state, and are checked again before
execution. Infrastructure target detection, dry-run gates, pre-change capture
and rollout checks use the remote environment; snapshots are retained privately
on the controller and staged privately outside the sandbox checkout for
rollback, so ordinary Git commits cannot accidentally include them.
Repo instructions come from the remote checkout; installed slash-command skills
stay on the controller. Sessions/memory are scoped by sandbox ID and workspace.
Model keys and host environment are never exported by hybrid. Use
`--project-id <stable-name>` to retain on/deep advisor lessons across replacement
sandboxes, and `--continue` to resume the latest local session for the attached
sandbox/workspace. These settings contain no credentials.

Hybrid owns only its local bridge. It does not stop, delete, reset or change
the lifecycle policy of an attached sandbox. Changes stay in that sandbox;
commit/export them with your normal Git or Daytona workflow. Ctrl-C reaches
the local rusty, which stops its turn; the bridge keeps serving. A remote
command may continue until its timeout; inspect its outcome before retrying.
There is no automatic replay or cancellation claim. The sandbox can continue
consuming compute credits after the CLI exits.

### Checks without cloud spending

```bash
cargo xtask qa
# Or only the cloud suites:
cargo test --test cloud --test cloud_hybrid
```

`tests/cloud_hybrid.rs` runs the real Rust CLI, real Rust tools, the bridge
and the real REST transport in separate local controller/sandbox directories,
against a fake Daytona toolbox API and a scripted model. Three workers overlap
in each profile; forged edits are denied; all eight file/search/Bash tools
operate remotely; the lead edits and verifies; careful has one checker; memory
on runs on the controller without hook timeouts. Additional checks cover
remote script inspection, changed approvals, unavailable transport (with a
credential canary in the API error), missing/invalid placement settings,
authentication, request bounds, attach-only lifecycle behavior, the Ctrl-C
shield, and remote infra gates/snapshots/verification.

`tests/cloud.rs` covers the full-cloud launcher: tracking, detaching and
re-attaching, export, privacy of exports, and every case that keeps the
sandbox; then `rusty-cloud` itself against a fake control plane, toolbox and
object store: sandbox creation payloads (keys only as sandbox variables),
signed context uploads and snapshot builds for `prepare` and
`prepare --source`, `prune`, and a hosted swarm journey with the real rusty
and rusty-memoryd in the stand-in sandbox. These establish local integration
behavior, **not** live Daytona or live-model qualification.

### Fully cloud agent

`rusty-cloud run` runs rusty in a Daytona sandbox and brings the results home.

```bash
export DAYTONA_API_KEY=...                       # or put it in .env
rusty-cloud prepare --source --cpu 2 --memory 4
# Copy the printed snapshot name into the run command.
# --source builds both Rust binaries remotely from tracked HEAD; no local Docker.
rusty-cloud run --snapshot rusty-<printed-hash> \
    --repo https://github.com/you/project --goal "get the test suite green" --mode careful
```

- **A pinned, prepared environment.** `prepare` builds a snapshot from a
  digest-pinned `debian:bookworm-slim` with git, ripgrep, python3 and make.
  `--source` builds rusty and rusty-memoryd with pinned Rust 1.90 Alpine,
  strips them, and includes MIT and dependency notices. Only archived HEAD
  runtime inputs (manifests, `src`, `skills`, `xtask` and the notices) are
  uploaded; `.env`, `.git` and untracked files stay local. Each `COPY` source
  goes to Daytona's build-context bucket as a tar, signed with the temporary
  credentials Daytona issues for it.
  The snapshot fingerprint includes source, recipe, resources and region
  (`rusty-<hash>`), so a run always starts from the same environment, and a
  new build gets a new snapshot. `prune` lists old `rusty-*` snapshots and
  deletes them only with `--yes`.
- **Tracked runs.** rusty runs in a background session in the sandbox. The
  run's id, sandbox, starting commit and command are saved to
  `cloud-runs/<run-id>/run.json` the moment the sandbox exists. Ctrl-C only
  stops following; `status`, `logs` and `stop` pick the run up again, and
  `list` shows every run started from here.
- **Results before cleanup.** When rusty finishes (or on `stop`), the launcher
  downloads these into `cloud-runs/<run-id>/`:
  - `patch.diff`, a binary-safe diff against the starting commit, ready for `git apply`;
  - `new-files.tgz`;
  - rusty's output and `--stats`;
  - the trajectory;
  - rusty's session state, without key files;
  - a `manifest.json` with sizes and hashes.

  The sandbox is deleted only after a confirmed exit and all essential exports
  arrive (patch, output, errors, trajectory, exit receipt, plus scoped memory
  when enabled). If the export fails, the sandbox is kept, and the launcher only ever deletes the sandbox labelled with its own run id.

Run state and exports use private directories (0700) and files (0600).
`--keep` keeps the sandbox after exporting. Keys reach the sandbox as
environment variables, never on a command line.

### NVIDIA endpoint access

The launcher checks endpoint transport (with `curl` in the sandbox) before
starting the agent, without sending a model key. This does not validate the
key or model availability.
Daytona Tier 1/2 blocks general outbound internet and rejects sandbox allowlist
overrides. NVIDIA's `integrate.api.nvidia.com` is blocked on the tested Tier 2
account. The documented solution is Tier 3, requiring a one-time $500 wallet
top-up. Support can be asked whether a scoped exception is possible; no such
exception has been granted. The launcher never upgrades or charges your account.
See [network limits](https://www.daytona.io/docs/en/network-limits/),
[tiers](https://www.daytona.io/docs/en/limits/) and
[troubleshooting](https://www.daytona.io/docs/en/troubleshooting/).
Do not disable TLS validation to work around connection resets.

Set `DAYTONA_TARGET=eu` before preparing and running to use EU. Snapshots are
region specific; reconnect uses the target saved in the run state. Resources
belong to `prepare`; each run inherits its snapshot's resources. For an
existing local Linux binary, `prepare --binary out/rusty` also packages the
sibling `rusty-memoryd` (run it from the repository root, where `LICENSE` and
`THIRD_PARTY_NOTICES.txt` are read). Snapshot names are the same ones the
earlier Python launcher computed, so snapshots it prepared are still found,
and its `cloud-runs/*/run.json` files still load.

### REST endpoints used

The calls mirror what Daytona's Python SDK 0.220.0 sends, read from its
source, with `Authorization: Bearer $DAYTONA_API_KEY`:

| Purpose | Request |
|---|---|
| snapshot lookup, list, delete | `GET /snapshots/{name}`, `GET /snapshots?page=&limit=`, `DELETE /snapshots/{id}` |
| snapshot build | `GET /object-storage/push-access`; S3 `HEAD`/`PUT <bucket>/<org>/<md5>/context.tar` (SigV4); `POST /snapshots` with `buildInfo.{dockerfileContent,contextHashes}`; poll `GET /snapshots/{id}`; follow `GET /snapshots/{id}/build-logs-url` |
| sandbox | `POST /sandbox` (`snapshot`, `env`, `labels`, `autoStopInterval: 0`, `target`); poll `GET /sandbox/{id}` until `started`; `GET /sandbox/{id}/toolbox-proxy-url`; `DELETE /sandbox/{id}` |
| toolbox, at `<toolbox url>/<sandbox id>` | `POST /process/execute` (`command`, `timeout`; never `envs`); `POST /process/session`; `POST /process/session/{id}/exec` (`runAsync`); `POST /files/upload-v2?path=`; `GET /files/download?path=` |

Two choices differ from the SDK's own wrappers, though both endpoints are in
its 0.220.0 API client: files move with the single-file `upload-v2` (raw
body) and `download` endpoints instead of the multipart bulk ones, and each
context is uploaded with one signed `PUT` rather than a multipart upload.
None of this has been run against live Daytona yet.

### Swarm and memory

One lead uses `--agents swarm` to schedule bounded parallel read-only workers;
only the lead edits and runs mutating commands. This runs within one sandbox,
so workers share a checkout and local memory socket without extra network hops.
Use `--memory-mode` with a memory level (`recall`, `learn`, `reflect` or
`deep`) for the local advisor, and `--memory-input <scoped-export.json.gz>`
for explicit lesson transfer. Session state stays in RAM; lessons are exported
separately, never as a raw database/WAL.
`--model` selects a model on the configured `RUSTY_BASE_URL` endpoint.

```bash
rusty-cloud run --snapshot rusty-<printed-hash> \
    --repo https://github.com/you/project --ref <commit> \
    --agents swarm --memory-mode learn --mode standard \
    --prompt "Use three read-only workers to inspect the components, then implement and verify the fix."

# Live qualification: real sandbox, real model endpoint and credentials.
RUSTY_CLOUD_SNAPSHOT=rusty-<printed-hash> RUSTY_CLOUD_REF=<commit> \
    cargo test --test cloud -- --ignored live_swarm_journey
```

The offline journey (`hosted_swarm_journey_offline`) checks that three workers
overlap, forged worker writes are denied, real reads succeed, the lead writes
and verifies files, memory hooks meet their deadline, and exports are
hash-verified before deletion. It is not evidence of live model quality, and
live qualification remains blocked until endpoint access and credentials work.

## Existing Linux boxes over SSH

`rusty-cloud ssh` keeps model calls and memory on your computer and runs tools
on an existing Linux host. This works with SSH-accessible VMs or development
boxes without a provider SDK or a Daytona account. Provision the box yourself,
install this version of the Linux `rusty` binary on its PATH, and clone the
project there. The host needs Bash and GNU `timeout` (coreutils).

First connect with `ssh your-box` to authenticate and verify the host key.
Then, from your local Rusty checkout:

```bash
cargo build --release --bins --locked
target/release/rusty-cloud ssh --host your-box --workspace /home/user/project \
    --local-binary target/release/rusty --memory-mode learn
# Add --prompt or --goal for a headless run; --trajectory saves a local receipt.
```

Use an alias in `~/.ssh/config` for a custom port, identity, or jump host,
or pass `--ssh-config PATH` for an isolated OpenSSH configuration.
Each attachment allows at most four SSH connections at once; additional tool
requests wait for a slot within their deadline. OpenSSH connection sharing
can also be configured on that alias to avoid reconnecting for each tool call.
Authentication is noninteractive and host-key checking remains strict. Agent
and port forwarding are disabled; model credentials are removed from the SSH
child's environment. Scripts travel on stdin, not in command-line arguments.
The same authenticated localhost bridge, remote permission recheck, output
bounds, read-only workers, and workspace-scoped session identity used by
Daytona apply here. `/tools ssh` saves this placement for a future session,
which must also be launched through `rusty-cloud ssh`.

SSH only attaches tools: it does not create, stop, delete, or export the box.
Changes stay in the remote checkout. A lost connection or local interruption
can leave a remote operation's outcome unknown; inspect it before retrying.
Remote commands have a timeout, but an SSH failure is not proof that all
remote work was cancelled. The transport never automatically retries a write.

The controller needs network access to the configured SSH host and port
(default 22), or a reachable jump host configured in OpenSSH. HTTP proxy
environment variables do not configure SSH; use the alias's `ProxyJump` or
`ProxyCommand` when required. The destination's host key must already be in
the configured known-hosts file. Connection setup and keepalives are bounded,
but a disconnect does not roll back a command that already reached the box.

For Daytona, allow HTTPS access to its control plane and toolbox; snapshot
preparation also needs its object store. Local reasoning in `hybrid` and
`ssh` needs model-provider access from the controller, while a fully remote
`run` needs that access from the sandbox. A firewall or provider egress policy
can still block those endpoints; transport support cannot bypass that policy.

Exit 255 reports an SSH connection or command failure, 127 means Rusty is
missing from the remote PATH, and 124 means the remote timeout expired.

## Docker (any cloud, CI, Kubernetes)

```bash
docker build -t rusty .
docker run --rm -it -v "$PWD:/work" -e NVIDIA_API_KEY rusty
docker run --rm -v "$PWD:/work" -e NVIDIA_API_KEY rusty --yolo --goal "fix the failing tests"
docker build --target bin -o out .  # just the static binary
```

The Docker build retains Cargo registry and compiler caches across source
edits. All three binaries, including `rusty-cloud`, are available in the
`bin` export and the runtime image. The runtime includes OpenSSH for attached
Linux hosts. BuildKit is required for cache mounts.

## AWS

- **EC2 or Cloud9-style dev boxes:** copy the static binary from
  `docker build --target bin -o out .` (pick an instance that matches your
  build architecture) and keep the key in SSM Parameter Store:
  `export NVIDIA_API_KEY=$(aws ssm get-parameter --with-decryption --name /rusty/nvidia --query Parameter.Value --output text)`.
- **ECS / Fargate / CodeBuild for unattended goals:** push the image to ECR
  and run `rusty --yolo --goal "…"` as the task command, with the key injected
  from Secrets Manager as an environment variable. `--stats` writes a JSON
  summary to stderr for your logs, and `--trajectory /out/run.json` saves the
  full conversation.

## Devbox

[`devbox.json`](../devbox.json) gives a reproducible shell with Rust,
ripgrep and Python: `devbox shell`, then `devbox run qa` or
`devbox run rusty`.

## Benchmarks

rusty runs as a Harbor agent: `--yolo --goal` for autonomy, `--trajectory`
for a reviewer-readable transcript, and `--stats` for token accounting. The
trajectory keeps every message, including the ones compaction summarised away
(`archived_messages` counts them), and is refreshed at most every 30 seconds
during a run, so a harness that kills rusty at its timeout still gets a record.

### Preparation memory

File hashes and uploads stream bounded buffers. Each build-context archive is
spooled to a private, immediately unlinked temporary file through a 64 KiB
buffer. The payload hash is computed during writing, and the buffer is flushed
before the archive is rewound for upload. Allow temporary disk space for the
largest context. Archive bytes and context identifiers remain compatible.
