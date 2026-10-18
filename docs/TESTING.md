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

`.github/workflows/ci.yml` is the only workflow. Every PR, title/body edit,
and main push runs `check` and the full offline gate. Superseded code runs
cancel; metadata edits cannot cancel code checks. The cache includes compiler
identity and the lockfile. Main publishes caches; PRs restore them. Conventions
run in the same job, without a second runner.

Use Run workflow to select offline, perf, live or cloud. macOS is available
explicitly for platform qualification; no automatic OS/model/memory matrix,
nightly schedule, or paid call runs on a PR. The former eval and memory-live
workflows are consolidated here. Demo recording stays a local `cargo xtask
record` command; quality runs never commit demo assets or push changes.

The live lane defaults to three repair evals, one repeat and one concurrent
task. Request/time/token limits bound each Rusty process; they are **not a
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
and leaves a receipt. Process termination leaves running/not-run entries,
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
| MCP discovery/pagination/names, required tools, permissions, usable secrets | `src/mcp.rs`, `tests/cli.rs` | Real external MCP service receipts |
| Infra target, fresh plan, snapshots, audit, rollout checks | `src/infra.rs`, `tests/cli.rs`, `tests/cloud_hybrid.rs` | Disposable real infra and independently observed state |
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

`cargo xtask qa --full` is the CI gate (also the default). `--changed` selects relevant test binaries; runtime/shared/unknown changes expand to full QA. A partial run is labeled changed-only and does not qualify the whole application. `cargo xtask flaky --runs 10` retains per-run failures/timings in `target/flaky.json`. `cargo xtask worktree BRANCH` prints setup for a compiler-specific shared cache; the `scripts/cargo-shared` wrapper locks the whole command across worktrees.
