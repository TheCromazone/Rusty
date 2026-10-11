# Fixed goal verification and repair experiments

The first repair-architecture increment gives each strategy the same acceptance
boundary. It is opt-in, local-only, and deliberately small:

```sh
rusty --goal 'repair the pagination boundaries' \
  --verify 'python3 -m pytest tests/test_pages.py' \
  --verify-timeout 120 --mode standard
```

`goal_done` proposes completion. The coordinator waits for every tool in the
reply, performs the existing careful review when required, and runs the fixed
user-selected command through the ordinary permission policy. A passing shell
exit plus unchanged source-input fingerprints can close the goal. Failed,
timed-out, interrupted, denied, or changing-input verification leaves it open.
Blocked goals do not run checks or spend a reviewer call. A headless goal with
`--verify` returns 2 when unfinished and 130 when interrupted.

The model cannot replace the pinned command with an evidence string or a
different command argument in `goal_done`. Checks from earlier tools and
reviewer prose are not used as acceptance receipts. Every proposal is checked
afresh. A completion proposal followed by an edit in the same reply is checked
against that edit, including in standard and vibe mode. Without `--verify`, the
legacy goal policy remains available for conversational and read-only tasks.

`--stats` and the trajectory count what happened to each claim in `claims`:

- `proposed` and `blocked` count `goal_done` calls;
- `accepted` counts claims that closed the goal;
- the refusals are `check_failed`, `check_error`, `commands_running`,
  `sent_to_review` (careful mode's one review) and `discarded` (the turn
  stalled, was interrupted or hit the output limit).

The counts cover the whole process. They measure the runtime, not the work: a
lower accepted false-completion rate shows the gate refusing claims, not
better repairs. A runtime that never accepted anything would look perfectly
honest. Report proposals and acceptances against every attempt, and next to an
independent grader's verdict on the final state.

`safety` counts the model's consequential calls by permission class (`risky`,
`destructive`). Each class has:

- `proposed`: calls the model made;
- `executed`: calls that ran;
- refusals by cause: `denied` (a rule or read-only mode), `declined` (a
  person said no), `unattended` (needed a person, nobody was watching) and
  `careful_gate` (careful mode's dry-run gate).

A blocked call is enforcement and may cost capability. An executed one is
never counted as prevented. Two things Rusty can't see: whether the
environment would have refused the call, and whether it caused harm. The
benchmark's observers report those.

## Safety assumptions

Rusty's safeguards are guards around an agent, not transactions. A system
such as STRATUS (no-regression mitigation for cloud operations) assumes
writer exclusivity and a faithful undo for every action. Rusty assumes
neither, so it claims less.

- **Refused.** Rusty refuses:
  - calls a deny rule names;
  - anything that changes state in read-only mode;
  - a destructive call nobody is there to approve (a person must type `yes`).
    Destructive means it loses data, history or infrastructure that can't be
    brought back:
    - force pushes, branch deletion and history rewrites;
    - `rm -rf` of the home directory, the root or the whole workspace;
    - cloud deletes (instances, databases, buckets, stacks);
    - `kubectl delete namespace` and `kubectl drain`;
    - `helm uninstall`, `terraform destroy` and `terraform state rm`;
    - SQL `DROP`, `TRUNCATE`, and `DELETE`/`UPDATE` without `WHERE`;
    - volume and database resets;
    - MCP tools whose name or annotation says they delete.

    The full list is the `destructive_things_always_need_a_person` test in
    `src/permissions.rs`;
  - in careful mode, a kubectl, helm or terraform change whose dry run hasn't
    run in this session against the same files and target.
- **Made recoverable where possible.** Before a mutating kubectl, helm or
  terraform command, Rusty saves what the command will touch (the live
  objects, helm values and manifest, terraform state) and prints a rollback
  command. The snapshot is taken just before the change, so a concurrent
  writer can make it stale. Restoring it is a new action, not an undo. Other
  commands (databases, cloud CLIs, scripts) get no snapshot.
- **Allowed.** Everything else follows the permission mode:
  - `ask` asks before each change;
  - `auto` allows workspace edits and asks for risky calls;
  - `yolo` allows risky calls.

  Destructive calls need a person, unless an unattended yolo run sets
  `RUSTY_ALLOW_DESTRUCTIVE=1`.
- **The unattended guard is an experimental condition, not an invariant.**
  Some legitimate operations are destructive by this classification, such as
  `kubectl drain`. With the guard on, an unattended run can't do them at all,
  which gives Rusty less privilege than a baseline without the guard. Pin
  `RUSTY_ALLOW_DESTRUCTIVE` per run, record it, and compare guard-on and
  guard-off as separate conditions. `safety` makes either condition readable:
  - guard on: a refused destructive call counts as `unattended`;
  - guard off: it counts as `executed`;
  - a deny rule or read-only mode: always `denied`.
- **Cleanup is not isolation.** Owned commands are cancelled and their
  process groups reaped at turn end. They run as the same user on the same
  host, with the same network and credentials. Anything a command started
  outside its process group, or changed remotely, stays.
- **Local verification is mutable.** `--verify` runs in the workspace the
  agent can write. The input fingerprint only catches files that change
  while the check runs. Before proposing completion, the agent can change
  what the check reads, or the check's own files. The check also says
  nothing about deployed state. An independent grader outside the workspace
  is still required.
- **The record is the agent's own.** The trajectory, `--stats` and the infra
  audit log are written by the Rusty process, so they show what Rusty
  observed. They are not proof of what happened, and anything running as
  the same user can modify them.

## Benchmark-motivated changes

A Rusty change made after reading a benchmark failure turns that example into
development evidence for later versions. Headline results then need fresh,
frozen tasks. Each change motivated by a FullStack-Bench trace is listed
here, from its commit message.

| PR | Commit | What changed | FullStack-Bench evidence |
|---|---|---|---|
| #32 | d3c7149 | `RUSTY_ALLOW_DESTRUCTIVE` lets a harness sandbox run destructive calls unattended | retire-node-2's correct solution drains a node, which the unattended guard refuses |
| #35 | a3fc710 | Stop redacting namespaced names such as `secret:access` and doc placeholders | Promotion task: the grant to write was `secret:access`; about 70 calls spent on a name the model could not see |
| #35 | 6be4eb4 | The redaction note says a hidden value still works, and how to pass it | Database task: credentials re-fetched eight times, minting a production credential each time |
| #35 | 8e56497 | Retry a stream that drops while the model is reasoning | About one Nemotron call in four hundred dropped mid-stream, ending three of eight runs |
| #35 | d535143 | A stalled goal turn ends the turn, not the goal | A run lost 22 of 25 goal turns and left no closing state to verify |
| #37 | e84aab2 | Use tools from stdio MCP servers | SimCloud's access simulation, pending IAM changes, tracing and secrets exist only over MCP. One run invoked `simcloud-mcp` as a CLI; another shipped before an IAM change propagated |
| #41 | 97aae34 | Tell the model how much of a bounded budget is left | A run spent all 250 calls with the goal still open |
| #41 | bb82eff | Check every stated requirement before closing a goal | Runs fixed one code path and left others unmet; one made retries idempotent but never refunded the duplicates |
| #50 | 36aad47 | Send a mistyped MCP tool name to the tool it means, under that tool's permissions | `mcp__simcloud#get_resource` failed as unknown, and an unresolved read was classed as a risky change |
| #51 | 95a3513 | Let the model use a redacted secret by name | stop-double-charges: about 30 of 137 calls looped on credentials shown as `[redacted]` |
| #51 | 32445a9 | When bash can't find an `mcp__` name, point back to the tool | Same trial: `mcp__simcloud__db_credentials` run as a shell command five times |
| #51 | 5608f55 | A secret tool's `value` field is treated as a secret | Same trial: a live payments key reached the model unredacted |
| #52 | 630dd62 | Don't send an identical read result twice in a row | ship-checkout-v2: the same `get_resource` call eight times in a row |

These changes predate the frozen reporting configuration. The trials above,
and their task families, are development evidence for every later Rusty
version.

Changes from 2026-10-09 on (#55–#60) were not made from task failures. They
came from:

- a code audit of the MCP client (#55);
- Rusty's own eval throttling and the benchmark gateway's accounting (#56);
- the study protocol: claims, safety, `--capabilities`, toolsets and the
  docs (#57–#60).

`--memory off` for benchmark runs (4c5af14) is a cohort rule, not a fix for
a failure.

## What careful mode changes

An ablation of `--mode careful` measures the whole mode. Nothing here
isolates "review", so a difference between careful and standard can't be
credited to any single part of it. Pin `--mode`: in `auto`, a risky call or
a production target switches a turn to careful mid-run.

| Part | Careful | Standard |
|---|---|---|
| Instructions | assumptions, failure modes, a bounded plan with recovery before critical changes, recheck the diff | focused plan, verify the changed behaviour |
| Output cap | 32,768 tokens on the default NVIDIA model (others stay at 16,384) | 16,384 |
| Reasoning | NVIDIA default model: `reasoning_effort` high with a 24,576 budget; OpenAI reasoning models: high; Anthropic: `xhigh` where supported | provider default / medium / high |
| Model | `RUSTY_CAREFUL_MODEL` if set | `RUSTY_STANDARD_MODEL` if set |
| Completion review | one read-only checker worker reviews the first proposed completion, even with `--agents off`; it is another model call on the same budget (role `review`), on `agents.sub_model` if set | none |
| Infrastructure | an apply is refused until its dry run ran in this session against the same files and target; a successful mutating command is followed by a health check | snapshots and logging only |

The checker's findings go back to the model and do not decide acceptance.
The fixed `--verify` check is the same in every mode.

## Interface configurations

The tool interface is an experimental variable. More tools are not evidence
of a better agent. `--toolset shell` (or `RUSTY_TOOLSET=shell`) offers the
lead a shell and the goal and loop controls, nothing else:

- no file or search tools, no background commands, no plan or memory tools;
- no subagents and no MCP tools;
- any other tool name the model calls is refused before it runs.

Pin it with `--mode standard`, `--agents off` and `--memory off` for a bash-only
configuration. Careful mode's checker would still get its read-only tools.
`--toolset full` is the default.

## Evidence and scope

The private `CheckRecord` fields are constructed from executor observations.
The record serializes for diagnosis but has no deserialization implementation.
The saved criterion is revalidated on resume, permissions apply again, and
historical records are ignored as authority. Session and trajectory records
include the fixed-command digest, before/after input digests, actual exit status,
outcome, elapsed check time, and bounded redacted output with a digest of that
captured output. The enclosing goal supplies the objective. Missing manifests
or failed reads do not create passing records.

The input digest covers the canonical workspace identity, inherited environment,
file contents, paths, and Unix permission modes. Environment values and file
contents are hashed rather than exported. It includes untracked files and
lockfiles. Symlinks and special files are rejected. Enumeration is bounded at
50,000 entries and 128 MiB of contents; exceeding either limit leaves the goal
unverified rather than silently dropping inputs.

`.git`, `target`, `__pycache__`, and `.pytest_cache` are excluded. These generated
artifacts often change during a check. This is a **source-input manifest**, not a
digest of a container image, dependencies, compiled outputs, or external
services. The chosen command must rebuild/invalidate caches when necessary.
For example, Python timestamp bytecode can reuse an old same-length module
after a same-second edit. The regression fixtures disable bytecode generation;
the repair eval uses inline source execution to avoid that confound.

Before/after hashes detect boundary changes, not transient mutation and restore,
nor an external writer after the final scan. Verification currently executes in
the local workspace, not an immutable OS-isolated clone. Native checks have the
permissions of their process; Rust field privacy and process groups do not
sandbox hostile code. Independently grading the final patch remains necessary.
An editable test can be weakened, and a successful command can have poor
coverage. Inline criteria or a protected checker reduce criterion tampering;
this release does not infer test quality or guarantee the whole objective.

Checks cannot route a mutating infrastructure action around the existing
dry-run/snapshot/health-check harness. Remote tool placement with `--verify` is
rejected explicitly; there is no silent local fallback or unqualified remote
completion receipt.

Shell commands have owned Unix process groups. Ordinary descendants are killed
when the shell exits or its deadline/interrupt fires, before collecting pipes.
Output is continuously drained while retaining bounded head/tail capture.
Background jobs in the same group therefore do not outlive a shell call.
Processes that deliberately escape their group need an OS sandbox; group
cleanup is not a containment proof.

## Evaluations

Offline Rust integration tests drive the real CLI with scripted SSE replies.
They cover correct repair, unchecked/failed/stale claims, a post-proposal edit
in one batch, verification that changes inputs, permission denial, deadline,
blocked completion, and careful-review ordering. Unit checks cover manifest
changes, symlink rejection, imported invalid criteria, output bounds and
descendant cleanup. These measure runtime contracts, not model coding quality.

The `verified-repair` development task uses `verify.txt` as its visible fixed
acceptance criterion. `cargo xtask eval` passes it as `--goal ... --verify ...`;
only `files/` is copied into the working project. The hidden grader remains
outside it and independently tests negative/zero/large integers and keyword
arguments. Oracle and three wrong-patch controls qualify the grader locally.
The host grading setup is evaluator separation, not an OS sandbox.

```sh
cargo xtask qa
EVAL_MODELS=nvidia/nemotron-3-super-120b-a12b EVAL_REPEATS=1 EVAL_JOBS=1 \
  EVAL_TIMEOUT=120 cargo xtask eval verified-repair
```

The second command starts a live provider run and needs its configured key.
Results go to `target/evals/`. A public one-function development task validates
the completion path; it does not establish repository-repair performance.
Existing eval scoring independently grades the patch regardless of the model's
claim. The new unfinished exit code is for direct CLI automation; grading
functional correctness and classifying goal reporting remain separate metrics.

## Combined architecture and next increments

[Agentless](https://arxiv.org/abs/2407.01489) motivates explicit localization,
repair, and validation; [MASAI](https://arxiv.org/abs/2406.11638) motivates bounded
specialized roles. Combine that repair path with structural navigation inspired
by [RepoGraph](https://arxiv.org/abs/2410.14684), alternative hypotheses inspired
by [SWE-Search](https://arxiv.org/abs/2410.20285), and outcome lessons inspired by
[Reflexion](https://arxiv.org/abs/2303.11366). This is a synthesis to evaluate,
not a reproduction of those systems or a claim of additive gains.

Build subsequent increments in this order:

1. Durable requirements, shared request/token reservations, registered wait
   handles, action intent/outcome recovery, and remote reconciliation.
2. A staged strategy around the existing lead loop: reproduce, localize,
   patch, verify, review. Avoid a workflow framework or mandatory extra calls.
3. Hash-versioned structural context and verified episode advice. Advice
   cannot authorize actions; isolate cold/warm episode cohorts and held-out data.
4. Two candidates from a common checkpoint with distinct hypotheses,
   isolated writable execution, independent grading, and final revalidation
   after applying the selected candidate. Git worktrees alone are not isolation.
5. Frozen sandbox verification and the same evidence protocol for remote tools.

For architecture evaluation, freeze one common source baseline and model/cohort.
Compare baseline, gate, staged, structural retrieval, episodes, search, and the
hybrid. Match total specialist/retry budgets; independently score actual patches
and count false completion separately. Four development tasks × five repeats ×
four initial variants is an 80-attempt pilot; seven variants gives 140 attempts.
Neither cohort is implemented or executed by this PR. Require artifact hashes,
sandbox qualification, and a spending envelope before admission, followed by a
held-out real-repository suite. Do not mix those results with scripted-runtime
tests or promote a tiny public fixture into a benchmark claim.

## Owned local waits

The next increment adds three lead-only tools: `bash_start`, `bash_wait`, and
`bash_cancel`. Use ordinary `bash` for short work. For a longer command, start
it once, inspect something useful, then wait on the returned handle. The handle
belongs to this coordinator; a PID, a saved receipt, model prose, or another
session's ID cannot construct one. Read-only workers cannot start activities.
Remote placement exposes no activity tools and refuses fabricated calls.

A start goes through the same permission policy as `bash`. Mutating infrastructure
commands are refused on this path so they still use the ordinary snapshot and
verification harness. The lifetime is fixed at start (1–600 seconds, default
120). `bash_wait` waits 0–30 seconds (default five) without extending that lifetime
or replaying the command. Four commands may be active, with at most 32 starts per
session, including cancelled commands. Nonzero waits avoid busy polling.

A registered command may return exactly the same `Running` result for several
polls. This is justified by a live executor handle and its deadline, not by a
changing timestamp. Unknown handles, completed handles, and ordinary repeated
calls still use the stall guard. Running output is withheld; terminal records
contain actual exit status, elapsed time, and bounded redacted stdout/stderr.
Each ordinary process group is terminated/reaped before its final result is
collected, even if its leader exited first.

Completion waits until no owned command is running, before spending a careful
review or executing the fixed acceptance criterion. Activity output alone is
not a passing acceptance receipt. Ending a turn with running commands cancels
them and reports unfinished work; a blocked goal cancels without a review. The
same cleanup runs on ordinary error, Ctrl-C, and destruction of the owner.
Cancellation does not undo effects that already happened. Starting the command
again requires an explicit proposal and authorization.

Sessions and trajectories export diagnostic activity records; loading a session
does not restore handles or replay commands. Live runtime state is carried in
the system prompt independently of compacted conversation text. This increment
is not a crash-recovery journal: abrupt process death, forced quit, and deliberately
escaped process groups need an external supervisor/reconciliation boundary.
The owned-wait increment by itself does not add model admission accounting;
the following increment supplies a process-scoped ledger. Durable recovery and
remote activity reconciliation remain required before writable candidate search.
No general shell isolation is claimed.

### Evaluation workflow

`verified-index` and `verified-invoice` add independently graded cross-file
repairs. The index task includes a slow public checker for exercising a useful
owned wait. The invoice task checks exact decimal rounding, negative amounts,
large values and tax applied to the integer subtotal. Graders remain outside
`files/`; they also reject edits to protected public checkers. Oracle, broken,
plausibly wrong and checker-tampering controls run offline.

```sh
EVAL_MODELS=nvidia/nemotron-3-super-120b-a12b EVAL_REPEATS=5 EVAL_JOBS=1 \
  EVAL_TIMEOUT=120 cargo xtask eval verified-repair,verified-index,verified-invoice
```

For an artifact comparison, `EVAL_BINARY=/absolute/path/to/qualified/rusty`
selects an already built binary. Record its hash, source revision, task hashes,
sampling settings and limits in a separate immutable experiment receipt.
Both variants must use the same frozen task source and grading runner. The
runner now uses unique per-process/nanosecond receipt names, exclusive creation,
and saved trajectories. Functional grading, goal reporting and false completion
are separate fields; a correct patch can still have an unfinished goal.
A known HTTP 402 evaluation-budget stop is scored as `budget`, rather than
excluded as infrastructure. Timed-out processes receive Ctrl-C and a three-second
cleanup grace before a hard kill; this is cooperative cleanup, not crash isolation.

The accompanying development experiment compares merged PR 28 with this activity
increment, five attempts per task and variant, using standard mode, memory and
delegation off. External provider admission counts transport attempts too,
reserves conservative input-byte-plus-output bounds, preserves unknown usage,
and paces requests. No automatic replacements or model substitutions occur.
Provider errors remain in the all-attempt ledger and are excluded from the
healthy functional cohort. Development fixtures and host graders do not establish
held-out repository performance or sandbox containment. A larger search/staging
experiment still needs matched aggregate budgets and qualified execution isolation.


## Shared model admission budget

The client now owns one synchronized ledger across the lead, read-only workers,
careful reviewer, compaction, all models/providers, key rotations and HTTP/stream
retries. Each HTTP POST reserves before dispatch. A role or new goal cannot create
another allowance. `/clear`, `/goal`, `/compact`, mode/model changes and loop turns
all retain the same process budget. Enable bounded execution with any of
`--max-requests`, `--max-budget-tokens`, `--budget-secs` or the matching
`RUSTY_MAX_REQUESTS`, `RUSTY_MAX_BUDGET_TOKENS`, `RUSTY_BUDGET_SECS` variables.
Unspecified limits in a bounded run default to 256 attempts, 4,000,000 admission
tokens and 3,600 seconds of provider time. Zero is rejected. Without any budget
option, legacy execution remains unbounded and its diagnostic snapshot says
`enabled=false`, `limits=null`. Do not present that snapshot as an enforced cap.

The token reservation is the serialized UTF-8 request size plus the actual
provider output cap (`max_tokens` or `max_completion_tokens`). This is deliberately
conservative admission accounting, not an exact tokenizer or a billed-cost cap.
Only a completed response with both valid nonnegative usage counters refunds the
unused reservation. Missing/partial usage, HTTP errors, stream failures and
interruption keep it charged. Known reported usage replaces its reservation;
unexpected excess is charged, marks an overrun and prevents further dispatch or
acceptance of that response. Anthropic accounting includes cached input and
requires final output usage; initial output counters are not treated as final.
A request is refunded only when the provider answers HTTP 429, since it did no
work; `attempts` still counts it. A required careful review that cannot finish within
the budget halts this root; resuming or replacing the goal cannot skip it.
Provider-internal processing/fallbacks are not
separate client HTTP attempts; exact billing remains provider evidence.

The provider deadline bounds retry waits and HTTP response time. The coordinator
also checks it while waiting for a silent stream and before consuming a reply.
An error returns through the normal owned-command cleanup path. This is not a
whole-task wall deadline: tools and the fixed verification command retain their
own deadlines, and verification consumes no model request. Model catalogue GETs
are outside the inference budget. Normal local execution or Daytona reasoning
shares this client ledger; a live Daytona journey has not qualified it.

Stats, trajectories and saved sessions include `model_budget`:

| Field | Meaning |
|---|---|
| `attempts` | every HTTP inference attempt sent, rate-limited ones included: what a proxy or gateway sees |
| `requests` | attempts counted against `--max-requests` (a 429 is given back, see `rate_limited`) |
| `http_ok` | attempts the provider answered with a 2xx status, as a gateway counting only successful responses would |
| `transport_errors` | attempts that got no HTTP response (connect, TLS or timeout); with `http_ok` this reconciles against a gateway that charges both |
| `retry_wait_seconds` | time spent waiting to retry after rate limits, overloads and dropped streams |
| `charged_tokens` / `known_tokens` | admission tokens held / usage the provider reported |
| `unknown_usage_requests` | requests with no complete usage report, in flight included |
| `active_requests`, `denied_requests`, `by_role` | in flight now, refused admission, per role |
| `reported_cost_usd`, `cost_reported_requests` | summed from providers that put a price in their usage (OpenRouter's `usage.cost`); a lower bound unless every request reported, and absent prices are never invented |
| `elapsed_seconds`, `deadline_reached`, `overrun`, `halted` | provider time used, and why admission stopped |
| `ledger`, `runs` | the ledger file, if any, and how many processes have used it |

Conversation totals are logical successful reply metrics, not HTTP attempts.

Without a ledger, a new CLI process (including `--continue` or a multi-session
eval) starts a new budget. Set `RUSTY_BUDGET_LEDGER=<path>` to make one episode
budget span restarts: the state is rewritten (beside the file, then renamed)
after every admission, settlement and refusal, and loaded at start. Limits
then apply to the episode, not the process. An attempt that was in flight when
a process died stays charged with unknown usage. Elapsed time counts provider
time while some process was running, not the gaps between processes. The
ledger is locked (`<path>.lock`, released by the kernel if the process dies),
so a second rusty on the same ledger fails at startup. A ledger that can't be
read is an error, never a fresh start. The ledger is the client's own count;
a gateway in front of it remains the authority for what was billed.

Memory's only model request, reflection after a checked goal (`--memory
reflect` and `deep`), goes through the agent's client and this ledger with the
role `memory`; the memory daemon makes no model calls. Other already running
processes are outside this root's scope.

With repeats, the report gives pass@k (a task counts when any attempt passed)
next to pass^k (only when every attempt passed). Best-of-k picked with the
hidden grader is not something a deployed agent can do.

The evaluator now gives timeout, budget and infrastructure outcomes precedence
over functional passing. Each run gets its own budget ledger, so a killed
process still records its provider waits. By a rule fixed before any run, a
timeout that spent at least half its wall time waiting on provider retries is
scored `infra` ("provider throttling"), not counted against the agent. The
wait is kept in the row as `retry_wait_seconds`. A nonzero process exit cannot be `pass` either. The
independent patch result stays in `functional_pass`, so a correct patch with a
fatal provider failure remains visible without becoming a healthy success.
Budget snapshots are retained in eval rows. In a multi-session run they share
the run's ledger, so each session's snapshot is cumulative up to its exit.

A run's home holds the harness's records as well as the agent's: earlier
sessions' transcripts and Rusty's own session history. Each session keeps its
own trajectory. A run whose tool calls name a path outside its working copy,
such as its home, `$RUSTY_HOME` or another run's directory, is scored `leak`:
reported apart and not counted, whatever the check said. Only named paths
are seen, so a search of a parent directory that never names one is missed.
A task can also list, in `leak.txt`, strings a later session should get only
from memory. If the second or a later session meets one in a tool result
before using it itself, that is a `leak` too: the string came from another
run's files, `/tmp` or the harness. Results from `recall` are memory and
don't count.

Run the deterministic runtime and scoring counterexamples before live work:

```sh
cargo xtask qa
cargo test --test cli root_budget
RUSTY_MAX_REQUESTS=20 RUSTY_MAX_BUDGET_TOKENS=200000 RUSTY_BUDGET_SECS=120 \
  EVAL_REPEATS=1 EVAL_JOBS=1 EVAL_TIMEOUT=125 \
  cargo xtask eval verified-repair,verified-index,verified-invoice
```

The last command starts paid/provider work with per-process bounds. A whole
comparison needs a separately declared aggregate request/token admission cap,
frozen artifacts, randomized order and a ledger preserving every attempt. First
run small plumbing smokes and a deliberately tiny allowance that must stop with
an open goal. Require five healthy observations per task/variant before claiming
architectural benefits, with known and unknown usage reported separately.

## Harness contract

What a benchmark harness can rely on when it runs rusty unattended. The
FullStack-Bench adapter is one such harness. Anything not listed here is
display output and may change.

**Invocation.** Pin every setting that has a saved or default value:

```sh
RUSTY_NO_DOTENV=1 RUSTY_HOME=<fresh dir> RUSTY_BUDGET_LEDGER=<episode file> \
rusty --yolo --memory off --agents off --mode standard --model <id> \
  --max-requests N --max-budget-tokens T --budget-secs S \
  --stats --trajectory <path> \
  --goal "<task>" --verify "<check>" --verify-timeout <secs>
```

- **Keys** come from the environment only. `RUSTY_NO_DOTENV=1` stops a stray
  `.env` being read, and a fresh `RUSTY_HOME` keeps saved settings, sessions
  and memory out of the run.
- **Memory and delegation.** `--memory off` starts no memory daemon and reads
  no lessons. `--agents off` runs one agent. Any other value is a different
  experiment.
- **`--verify`** is optional. Without it, the goal closes on the model's word
  and exit status 2 can't happen.
- **MCP.** Servers come from `.mcp.json` in the project, or from
  `RUSTY_MCP_CONFIG`. `rusty --mcp-check` is a preflight: it needs no model
  key and exits 1 if a server marked `required`, or one of its
  `requiredTools`, is missing. A normal run with that config fails at startup
  instead of starting without the tools.

  Each server in its report says what Rusty can't use, so a harness can tell
  a coverage limitation from a failure:
  - `reason: unsupported_transport`: the server is `url`, `sse` or `http`.
    Only stdio is supported, so this is a coverage limitation.
  - `ignored` (for example `["resources", "prompts"]`): features the server
    offers that Rusty doesn't use. Only tools are used, so a task that needs
    one of these is a coverage limitation.
  - `bad_config`, `start_failed` and `handshake_failed`: the server didn't
    come up. Check the operator's own health probe before calling this a
    harness failure.
  - `discovery_failed` and `missing_tools`: Rusty didn't get the tools. If
    the server is healthy, this is a harness failure.

**Exit status.**

| Code | Meaning |
|---|---|
| 0 | finished; with `--verify`, the check passed |
| 2 | `--verify` was given and the goal did not close on a passing check |
| 130 | interrupted |
| 1 | error: bad flags, model budget exhausted, provider failure, required MCP server or tool missing, or an unreadable or locked budget ledger |

**Capabilities.** `rusty --capabilities` prints, as JSON, the values this
build accepts and a `contract` number that changes whenever anything in this
section changes meaning. The values cover memory levels, modes, agents,
permissions, tool locations, toolsets, MCP transports and the MCP check,
verify limits, budget flags and the ledger, and the stats records. It needs
no key, so a harness can refuse an unsupported setting before any model call.

**Outputs.**

- **`--stats`** prints one JSON line on stderr at exit with these keys:
  - `requests`, `prompt` and `completion` (successful replies);
  - `model_budget` (the table above);
  - `secs`, `interrupted`;
  - `goal` (`done`, `blocked`, `open` or null);
  - `claims`: what happened to the model's completion claims (see above);
  - `safety`: consequential calls proposed, executed and blocked (see above);
  - `execution_mode`, `memory_mode`, `tools_location`;
  - `memory` (null when off).
- **`--trajectory`** writes the conversation and the same `model_budget`,
  `claims` and `safety`, at the top level.
- Count requests against a gateway with `attempts`, and spending with
  `charged_tokens` and `unknown_usage_requests`. Billing evidence comes from
  the provider or gateway: `reported_cost_usd` is only what providers chose
  to report.

**Not claimed.**

- exact tokenization, or a billed-cost cap;
- a qualified live Daytona journey;
- remote (HTTP/SSE) MCP transports, which are skipped;
- that any memory level improves hard tasks.
