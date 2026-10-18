# Testing Rusty

One workflow, one entry point, no new test framework or runtime dependencies:

```sh
cargo xtask qa           # offline: format, lint, all tests, memory and real PTYs
cargo xtask qa --list    # print the exact plan without executing it
cargo xtask qa --perf    # also build release and enforce the published budgets
cargo xtask qa --live    # also run live CLI, memory, interrupt and selected evals
cargo xtask qa --cloud   # also run the real hosted swarm journey
```

Flags can be combined; the release binaries are built once. All Cargo build,
lint and test stages use the lockfile. Existing narrow commands remain useful
for diagnosis; CI invokes the same QA command developers run locally.

## CI policy

`.github/workflows/ci.yml` is the canonical workflow, including the scoped
OpenSSH and container gates. Opened, reopened and synchronized PRs
and main pushes run `check` and the full offline gate. Superseded code runs
cancel; title/body edits run conventions only in a separate concurrency group,
without cancelling code QA or selecting the expensive jobs. The cache includes compiler
identity and the lockfile. QA and conventions share an archive namespace, while
each runner keeps its own target directory. Main QA alone publishes caches;
PR jobs only restore them. Cache hits and time savings require hosted evidence. Conventions
run independently in `messages`, with its own PR concurrency group, so invalid
metadata does not prevent code QA and an older code run cannot replay a stale
metadata failure after a title fix. It reads the current title/body through
the read-only repository API, while commit checks use the event's immutable
base/head revisions. The small checker builds in parallel with
QA; this duplicates that package's build to keep the two results independent.
Edited events run only `messages`; the unrequested QA job uses a different
check name so it does not replace the preceding code `check` result.
The scoped `benchmark` job retains the fixed-input cloud/read comparison:
separate baseline/candidate targets, exact read/tar/context equivalence, and
CPU/RSS tables. Scoped jobs select the PR merge-base diff so a behind branch
does not rerun expensive gates for changes that only landed on main. It runs only for relevant PR paths, once per workflow; no
second cloud-memory workflow builds those releases. An unselected comparison
is reported as such. Documentation keeps its separately owned `documentation`
job when that feature is integrated. The `openssh` gate preserves the disposable
strict-limit server, authenticated readiness, real concurrency/tool/timeout/
revoked-auth checks and always-run cleanup. The `build` gate preserves runtime
image and three exported binaries, packaged SSH smoke, and a source-change
rebuild. That rebuild is a behavior smoke, not evidence of cache effectiveness;
no cross-run Docker cache improvement is claimed. Superseded code runs cancel
these gates through the workflow concurrency policy.

Use Run workflow to select offline, perf, live or cloud. macOS is available
explicitly for platform qualification; no automatic OS/model/memory matrix,
nightly schedule, or paid call runs on a PR. The former eval and memory-live
workflows are consolidated here. The manual `demo` suite preserves README GIF
recording and commits its output only to the selected non-main branch; ordinary
quality runs never record or push demo assets.

The live lane defaults to three repair evals, one repeat and four concurrent
tasks, with an explicit `jobs` input restricted to 1–8. Request/time/token limits bound each Rusty process; they are **not a
whole-workflow dollar cap**. Empty tasks explicitly selects all current evals;
repeat counts are limited to 1–5 in the UI workflow. Configure repository keys
only for the selected live step. Cloud requires `DAYTONA_API_KEY`,
`NVIDIA_API_KEY`, an existing qualified `cloud_snapshot`, and its immutable
`cloud_ref` matching this checkout's full commit SHA. The snapshot itself must
be separately qualified; a supplied ref is not a binary attestation. It creates
billable compute. Do not select it without that intent.
A controller timeout is not proof the remote sandbox was cleaned up.

## Evidence and release acceptance

`target/qa/summary.json` and `summary.md` show each gate's pass/fail/running/
not-run status and duration. A failed child exits nonzero, stops later gates,
and leaves a receipt. Test discovery must finish and find at least one binary.
Every libtest binary must emit a complete, consistent JSON suite, including
legitimate zero-test binaries. `target/test-suite.json` records passed, failed
and ignored counts plus per-binary and per-test durations. Captured Cargo,
libtest and doctest processes have a 16 MiB output limit and a ten-minute
command deadline. Cancellation stops their owned groups, with a two-second
termination grace before a final kill and reap. Fixtures that create a new
process group still own their explicit cleanup. Process termination leaves running/not-run entries,
not a success receipt. Reports record the source commit, dirty state and selected eval configuration; a local dirty run is
not an immutable release qualification. CI uploads small receipts for three
days, not all transcripts or every successful terminal capture. Failed local
PTY captures remain in `target/qa/terminal` for inspection.

An offline pass establishes runtime behavior with scripted providers. A live
pass establishes only the selected model/configuration/tasks. The cloud lane
establishes only its hosted swarm journey, **not** every provider, deployment,
rollback, or remote cancellation behavior. No lane promotes an unrun check
to evidence. There is no automatic deployment or merge.

For release/model comparisons retain binary and task/checker hashes, source
revision, model/provider, mode, memory, permissions, toolset, limits, all
attempts and independent final-state grading. Compare the candidate with the
previous artifact on frozen tasks. Five healthy repetitions are a starting
smoke cohort, not proof of production reliability. Report infrastructure,
leak, timeout and budget outcomes separately; do not silently rerun misses.
Use memory off except when measuring memory. The existing eval report groups
by model and memory, so keep other configurations in separate result sets.

`--perf` measures fixed offline workloads and fails against AGENTS.md's CPU,
RSS, throughput, catalog-size, per-turn and binary-size budgets. Total wall time is diagnostic; the published per-turn gap budget still
includes scheduling delay and should be measured on a quiet machine. Performance artifacts are platform
specific; use the same machine for before/after comparisons. An over-budget
baseline is a failure to investigate, not a reason to silently loosen limits.

## Feature-to-test map

These are executable test locations, not claims that each deployment has
already passed. `cargo test --workspace` includes every default Rust test;
ignored tests run only in the explicit extensions.

| Feature / contract | Offline gate and existing tests | Additional qualification |
|---|---|---|
| CLI, config, precedence, workspace snapshot, project instructions, toolset | `tests/cli.rs`, `tests/modes.rs` | Clean release installation on each supported OS |
| REPL, slash menu, queued input, themes, fonts, views, Unicode, resizing | `src/slash.rs`, display/UI tests; full terminal captures | Human inspect unusual terminals |
| Compatible/NVIDIA, Anthropic, OpenAI, Codex routing and history | `tests/cli.rs`, `src/llm.rs`, `src/anthropic.rs`, `src/codex.rs` | Live account journey for each advertised provider |
| Streaming, partial calls, retries, key rotation, refresh, cache tokens | Provider unit and real-binary protocol tests | Real refresh/cache/provider failure receipts |
| File read/write/edit, syntax protection, list/search/glob/outline | `src/tools.rs`, real-binary CLI and memory journeys | Hidden-grade multi-language repairs |
| Bash, bounded output, background handles, wait/cancel, process groups | `src/activity.rs`, `tests/cli.rs`; owned PTY captures | Observe remote execution separately |
| Permissions, deny precedence, destructive confirmation, wrappers/scripts | `src/permissions.rs`, `tests/cli.rs`, approval PTY | Adversarial service-side observers |
| Auto/careful/standard/vibe and read-only reviewer | `tests/modes.rs`, `tests/cli.rs` | Repeated equal-budget mode comparison |
| Plans, goals, fixed verification, same-reply edits, resume, stalls | `tests/cli.rs`, `src/verification.rs` | Immutable external grader; mutable workspace checks alone are insufficient |
| Loops and pacing | `tests/modes.rs`, agent tests | Long-running cancellation/soak |
| Subagents, swarm overlap, overflow, shared findings, worker authority | `tests/cli.rs`, `tests/modes.rs`, `src/swarm.rs` | Live worker/lead final-state grading |
| `/btw`, private follow-ups, no tool/goal authority | `tests/cli.rs`; aside PTY captures | Live aside answer quality |
| Persistent sides, focus, interruption, reopen, read-only capability | `tests/cli.rs`; side PTY captures | Live investigative quality |
| Explicit summary transfer, parent ownership, stale observations, dedup/crash | `tests/cli.rs`, `src/inbox.rs`; summary PTY captures | Long-session mixed-work soak |
| Atomic sessions, locking, index recovery, search, concurrent writers | `src/session.rs`, `tests/cli.rs` | Release artifact restart/crash journey |
| Context windows, reserves, cache usage, overflow, all compaction modes | `src/context.rs`, `tests/cli.rs` | Hidden constraints after repeated live compaction |
| Memory off/recall/learn/reflect/deep, CRUD, recall, scope, anchors, credit | `src/advisor.rs`, `src/recall.rs`, `tests/advisor.rs`; five-level memory smoke | Existing memory evals with off/on baselines |
| Memory daemon, concurrency, restart, import/export, poisoned/stale lessons | `tests/advisor.rs`, advisor tests and memory smoke | Live memory delivery/use/benefit/harm separately |
| Secret redaction, usable handles, session/catalog/export privacy | `src/privacy.rs`, `tests/cli.rs`, advisor/session tests | Canary scan all real-provider/export surfaces |
| Built-in/project/user skills, precedence | `src/skills.rs`, `tests/cli.rs` | Live task-specific behavior |
| Native Git status/diff/log, project remote selection, GitHub/GitLab/Bitbucket setup and reads | `src/integrations.rs`; real-CLI `native_integrations_*` tests with fake host CLIs/HTTP | Native account setup and reads on each advertised host, including self-hosted GitLab; local mock receipts do not qualify live accounts |
| MCP manifest add/list/remove/check, optional entries, env references, secret refusal, concurrent edits and permissions | `src/mcp.rs` config tests; real-CLI `unified_mcp_*` and concurrent configuration tests | Existing/shared manifest permissions and boundary checks on the delivered source; external server authentication remains separately observed |
| MCP discovery/pagination/names, required tools, permissions, usable secrets | `src/mcp.rs`, `tests/cli.rs` | Real external MCP service receipts |
| MCP frame/queue bounds, dispatch outcomes, cancel, reconnect without replay, reader/child cleanup | Focused `src/mcp.rs` regressions on the delivered MCP branch, then exact-binary effect/cancel/reconnect RSS/FD/process observations | Keep pipe SIGINT, raw TTY Ctrl-C/aside and native durable recovery separate; fake-CLI checker tests qualify only the checker, and bounded observations do not prove arbitrary servers leak-free |
| Infra target, fresh plan, snapshots, audit, rollout checks | `src/infra.rs`, `tests/cli.rs`, `tests/cloud_hybrid.rs` | Disposable real infra and independently observed state |
| SSH transport and packaged runtime | `cloud::ssh::tests::real_openssh_network` in scoped `openssh`; runtime image and three-binary export/SSH smoke in scoped `build` | Disposable local OpenSSH proves its fixture only; live user hosts and production deployment remain separate. Cache reuse needs separate measured evidence |
| Local/hybrid/full-cloud placement, bridge auth, no uncertain-write replay | `tests/cloud.rs`, `tests/cloud_hybrid.rs` | Explicit cloud lane plus real hybrid/rollback failure journeys |
| Shared budgets across retries/review/workers/compaction/sides/restart | `src/budget.rs`, `tests/cli.rs` | Bounded live accounting, not exact billing |
| Stats, claims vs acceptance, trajectories, refusal counters | `tests/cli.rs`, `xtask/src/report.rs` | Independent provider/tool observers |
| Interrupt, recovery, shutdown, selected-side cancellation | CLI/activity tests; Ctrl-C and side PTY captures | Live Ctrl-C lane; remote cancellation separately |
| Performance and packaging/license compliance | `tests/licenses.rs`; explicit `--perf` | Clean installed release and container artifact |
| Grader integrity, oracles, wrong repairs, infra/budget/leak precedence | `tests/cli.rs`, `xtask/src/eval.rs`, `xtask/src/report.rs` | Frozen hidden-check eval cohorts |
| CI selection, failure propagation, truthful evidence, budget thresholds | `xtask/src/qa.rs`, `xtask/src/bench.rs` | Real GitHub `check` run for the exact PR commit |

The existing 30 evals cover coding/search/refactoring, verified repairs,
multi-turn pivots/undo/goals/compaction, and memory use/staleness/poisoning.
The explicit live lane runs the selected subset, not an implied full feature
matrix. Provider, infrastructure and cloud rows with additional qualification
remain open until their independent receipts exist.

`cargo xtask qa --full` is the CI gate (also the default). Test time scale defaults to 1; another scale requires an explicit environment setting and is recorded in receipts. `--changed` selects relevant test binaries; runtime/shared/unknown changes select every test binary. A partial run is labeled changed-only and does not qualify the whole application. `cargo xtask flaky --runs 10` retains per-run failures/timings in `target/flaky.json`.


## Separate worktree targets

Use ordinary Git worktrees and keep each checkout's own `target/` directory.
Run Cargo directly rather than a shared-target shell function. Do not point
multiple worktrees at the same `CARGO_TARGET_DIR`; a previous checkout's
binary must never stand in for the source being tested.

The hosted `check` job on commit `191928c` (run 39110575738) completed in
309 seconds. The 15-minute offline limit leaves 591 seconds of headroom for
that observed run; it does not establish a cold-cache or scale-1 timing bound.
Recheck the margin after the scale-1 revision and compiler/cache changes.
