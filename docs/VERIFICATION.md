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
`--verify` returns 2 when unfinished and 130 when interrupted. A goal that
stalls three turns in a row returns 2 with or without `--verify`.

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
| #91 | 4a8e364 | A goal that stalls three turns ends unfinished (exit 2, `goal_end: stalled`), not with an error (exit 1) | M4 cohort at 32cac02: 9 of 63 attempts ended on the stall guard with exit 1; in rusty-combined / stop-report-connection-leak / seed 2 the verifier graded the work solved, and the error exit made it the cohort's invalid attempt |

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

After the reporting pin, compaction modes (`--compaction`, `/compact
settings`) were added at the owner's request, motivated by Fan et al., "An
Empirical Study of Harness Design for Coding Agents" (arXiv:2609.20804), not
by task failures. They are not in the reporting configuration. The default,
`standard`, keeps Rusty's 60% and 80% thresholds, and any other mode is a
separate experiment.

Context accounting (`--context-tokens`, the `context` record) followed at the
owner's request, motivated by the same study and by making compaction work
the same on any model. It changes where `standard` fires: thresholds now
count the system prompt and tool schemas, measured from the provider's
reported usage, where the pin estimated history alone.

What a compaction carries of the user's own messages changed after the pin
too, as a fix found by reading the code, not a task failure. The pin carries
the newest 8,000 characters and drops the block an earlier compaction
carried whenever the messages since then pass that cap, the first request
included. Now the first and newest messages always survive whole; over the
cap, older entries are cut to their first line, then middle entries are
dropped oldest first, newer ones before carried ones.

Smart compaction then kept the model's last two steps verbatim instead of
summarising everything, put a when-to-compact rubric in the tool
description, and gained harness-side refusals: an unresolved error, a
pending call or background command, and fewer than ten tool calls since the
last compaction. These follow the owner-approved compaction design, not task
failures. Smart mode is opt-in and outside the reporting configuration.

The fixed sizes then became proportional to the window, again by the
owner-approved design: the reply reserve (the mode's reply size, at most an
eighth of the window, at least 4k; 16k at 128k as before), the tool results
kept whole, the head a trimmed result keeps, the user messages a compaction
carries, and smart mode's floor (never below 4k tokens). `--history-cap-tokens`
caps the thresholds; unchosen, the cap is 128k above a 200k window whatever
the mode, and the mode follows the window (`aggressive` below 48k). A
request's output cap (`max_tokens`, `max_completion_tokens`) is the mode's
as before when the window has room for it beside the context, else the room
left less a fiftieth of the window, never below the reserve. A request
changes from the pin's only when its prompt and output cap came within a
fiftieth of the window or past it. At a 128k window with
`standard` chosen, only the carried messages change: a tenth of the history
budget where the pin carried 8,000 characters. Harness runs on these builds
should pin `--compaction` and, for windows over 200k, `--history-cap-tokens`
or accept the 128k default.

A later fix gave subagents and swarm workers the lead's compaction mode and
history cap. They had kept the defaults (`standard`, no cap), so under a 1M
window a worker on the lead's model summarised only near 787k tokens.

Two limits remain. An unchosen mode and the default history cap are picked
from the window at startup and kept when an overflow error later corrects
the window (an assumed 128k found to be 32k keeps `standard`), and `/model`
keeps the previous model's window and calibration. Both stay until reporting
runs show whether either matters; `--context-tokens` and `--compaction`
avoid them.

A later fix stopped a compaction from repeating on every step when it can't
get the context under the summary threshold, as with a window or
`--history-cap-tokens` too small for the system prompt and tools. The next
threshold compaction then waits until the context has grown by a tenth of
the threshold, and the user is told the window or cap is too small.

A later fix measured smart mode's 20% floor, its 25% invitation and the
"Context in use" note against the budget whose summary point is the history
cap, rather than the whole window. At a 1M window with the default 128k cap
the floor had sat above the summary point, so the model's own compaction was
always refused.

A compaction then started telling undone edits apart, again by the
owner-approved design: at startup rusty records each tracked file's blob
(`git ls-files -s`, with uncommitted files hashed as they stand), and at
compaction a "changed" path whose current blob equals that one moves to
"touched, now unchanged". It is deterministic and costs one `git
hash-object` per compaction. Untracked files and projects outside git are
left as they were. A file edited by absolute path is never moved, because
the startup record is keyed by paths relative to the project, so it stays
"changed": a missed collapse, never a false one. A path containing ", " is
re-read as two names by a later compaction's working set.

`--compaction-model` (`RUSTY_COMPACTION_MODEL`, `compaction_model` in saved
settings) then let a separate model write compaction summaries, again by the
owner-approved design. Unset, the session's model writes them as before. A
summariser that errors or returns an empty summary hands that compaction to
the session's model once. At startup a summariser no endpoint serves is
refused, and so is one whose window (from its endpoint or the saved table)
is smaller than the largest summary request, the summarise threshold plus
the reply reserve (careful's, unless `--mode` pins another, since auto can
switch to careful); an unknown window is reported and left to the fallback.
The summariser's reported usage feeds neither the context meter nor its
calibration, so a different tokenizer cannot skew the session's fill, and
its tokens are totalled under its own model.
Each compaction records the model that wrote its summary, so a comparison
can tell a summariser's effect from a fallback. It is a separate
configuration: pin it, or leave it unset, and don't pool runs across it.

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

Run `cargo xtask qa` before a push on an otherwise idle host. The memory
service's control calls keep a 1000 ms budget, and under load from concurrent
eval runs a large export can overrun it; a QA failure on an idle host is real.

The second command starts a live provider run and needs its configured key.
Results go to `target/evals/`. A public one-function development task validates
the completion path; it does not establish repository-repair performance.
Existing eval scoring independently grades the patch regardless of the model's
claim. The new unfinished exit code is for direct CLI automation; grading
functional correctness and classifying goal reporting remain separate metrics.

### Predeclared: loss after compaction (2026-10-13)

`DECISION:` lines (decisions kept verbatim across compactions) stay unbuilt
unless development runs show a loss after compaction. The reporting cohort at
the pin never compacted, so it gives no evidence either way. The runs are
`compaction-rule` and `compact-focus` on main 1bffef1, memory off, the default
compaction mode, five repeats each, `EVAL_ISOLATE=1`, `EVAL_TIMEOUT=1800`,
nemotron-3-super. Written before the first repeat starts.

A run shows a loss after compaction only when all three hold:

1. a compaction boundary (the `compactions` record) falls between the turn
   that states the rule or fact and the turn that needs it;
2. the summary that replaced history omits it: the summary message at
   `boundary` lacks the token itself, `RUSTY:` for `compaction-rule` and each
   of `37`, `905` and `TimeoutError` for `compact-focus`, so a summary that
   paraphrases the item still counts as carrying it;
3. the check fails on that item (the `RUSTY:` docstring, or a wrong value in
   `ANSWER.txt`), not on unrelated work.

A failure without a boundary in between, or with the item present in the
summary, counts against `DECISION:` lines, not for them. The report keeps two
outcomes apart: a dropped user instruction (both tasks state theirs as the
user) argues for keeping user instructions verbatim and says nothing about
agent decisions; only a dropped decision the agent itself stated argues for
`DECISION:` lines.

**Outcome: parked, unbuilt.** In the predeclared runs at 1bffef1, 5 compactions
in 2 of 10 runs all fell between the rule and its use and all kept it;
`compact-focus` never compacted (5/5 refused as too small). The
`compaction-rule` task also shows the rule in its working copy, so a drop
could have been masked. No run tested an agent-stated decision. The question
is open, not answered no: no loss was observed, which is not evidence
against. Two `compaction-rule` runs failed when the model read `util.py` four
times in a row and the repeat guard ended the turn; that is model behaviour,
recorded with no change.

If item 8 is reopened, the task to build is one where the agent states a
decision mid-run that nothing in the working copy or the user's turns
restates (for example, choosing between two equal designs and committing to
one), a compaction is forced between the decision and the step that depends
on it, and the check reads that dependent step. It is predeclared the same
way.

### Predeclared: a lesson whose anchor is gone (2026-10-13)

In `memory-only-fact`, session 1 captured the seed command in every `learn`
run, and whether session 2 got it depended only on the lesson's anchor: a
lesson anchored to the onboarding note was never offered once the reset
deleted the note (freshness 0). The candidate change, V1, keeps a lesson's
score when an anchored file is missing and adds a warning to the advice
(`Changed since: ONBOARDING.md (gone); re-check before relying on it.`). A
changed file still scores 0.5, and the learn threshold is unchanged. A first
candidate that scored a missing file 0.5 was dropped before any run: a lesson
the agent saved then falls below the learn threshold and is still not
offered.

Runs, written before the first repeat:

- `memory-anchored-fact`: the harness seeds the seed command as an agent
  lesson anchored to `ONBOARDING.md` (`rusty-memoryd remember --file`), the
  reset deletes the note, and session 2 passes only with the remembered seed.
- `memory-obsolete-fact`: the same lesson, but the reset also rotates the seed;
  the current one is in `CONTRIBUTING.md`, and session 2 passes only with it.
- Baseline (main with these tasks) against V1, `learn`, five repeats per task,
  nemotron-3-super, `EVAL_ISOLATE=1`, `EVAL_TIMEOUT=1800`, one job per key.
- Regression checks on V1: `stale-anchor` and `poisoned-lesson`, three repeats.

Reading, decided now:

- A run acts on the stale value when the old seed appears in any command that
  runs the suite, or is written into the deliverable; it is counted once per
  run, not per command.
- Baseline offers nothing on `memory-obsolete-fact`, so its stale-value count
  is expected to be 0 of 5, and V1 must also be 0 of 5: a single stale-value
  run under V1 is a fail, not a tie. If baseline shows stale-value runs, the
  bar is baseline's count.
- V1 is merged only if `memory-anchored-fact` improves on baseline (the lesson
  offered with the warning, and used) and `memory-obsolete-fact` meets that
  bar, and the regression checks do not get worse.
- If V1 fails `memory-obsolete-fact`, the fallback (offer a gone-anchor lesson
  only to a `recall`) is run only if a session calls `recall` at all. No
  session has so far, so it would leave `memory-anchored-fact` at baseline;
  without a recall, the anchor change is parked, the result recorded, and main
  left as it is.

**Outcome: V1 parked; main unchanged.** Baseline 0b3e64c against V1 655bad4,
as predeclared. Each cell is pass / fail / timeout / leak:

| | baseline | V1 |
|---|---|---|
| `memory-anchored-fact` | 0 / 3 / 0 / 2 (lesson offered 0/5) | 5 / 0 / 0 / 0 (offered 5/5, at the first request) |
| `memory-obsolete-fact` | 5 / 0 / 0 / 0, stale 0/5 | 5 / 0 / 0 / 0, stale 4/5 |

V1 delivers the lesson, and `memory-anchored-fact` improves from none of three
scored runs to five of five. It fails the harm bar: in four of five
`memory-obsolete-fact` runs the first suite run used the remembered seed after
the warning, before the model re-hashed, found the rotated seed in
`CONTRIBUTING.md` and passed. Those four count as stale, as decided in advance,
though all four recovered. No `learn` session called `recall` in either build
(the only recalls in these experiments were memory-off sessions offered the
tools before #92), so the fallback cannot help, and the change is parked. On
V1, `stale-anchor` passed 3/3 and `poisoned-lesson` 2/3; that task passed
about two in three at every level, memory off included, in the earlier
matrix, so its miss is not attributed to V1. A later variant, such as advice that says to
verify the value against the working copy first, would be a new predeclared
test, not a reading of this one.

### Predeclared: a lesson whose anchored file changed (2026-10-13)

The open item in MEMORY.md: a lesson the agent saved starts at confidence
0.36, so at `learn` a changed anchor's freshness of 0.5 puts it under the
threshold, and the `Changed since` warning only ever reaches a lesson the user
saved. Two candidates, committed now (C1 43455d4, C2 ebc522d on 82342e4) and
rebased onto the merged main before the runs, which changes their ids but not
their diffs; the run logs record the ids:

- C1: freshness ranks lessons but no longer decides whether one is offered.
  A lesson whose anchored file changed is offered, with the existing warning
  (`Changed since: ONBOARDING.md; re-check before relying on it.`), when
  relevance × confidence clears the threshold. A missing file still hides it.
- C2, C1 plus wording: the warning reads `Changed since: ONBOARDING.md; read
  the current version first and follow it where it differs from this
  lesson.` It changes the warning for user lessons too.

Checked before any run, with the tasks' lesson and request against the
advisor: the baseline offers nothing after either reset; C1 and C2 offer the
lesson with their warning after both; all three offer it unchanged when the
note is untouched, and none when it is gone. Saved by the user, the same
lesson is offered after both resets by all three builds, with the old warning
from the baseline and C1 and the new one from C2.

Runs, written before the first repeat:

- `memory-changed-fact`: the harness seeds the seed command as an agent
  lesson anchored to `ONBOARDING.md`; the reset rewrites the note without the
  run command and keeps the seed, so session 2 passes only with the
  remembered seed.
- `memory-changed-obsolete`: the same lesson; the reset rotates the seed and
  writes the current one into the rewritten note, so session 2 passes only
  with the note's seed.
- `memory-changed-obsolete-user`: `memory-changed-obsolete` with the lesson
  saved by the user, which every build offers with its warning. It tests C2's
  wording on user lessons; for those, C1 behaves as the baseline.
- Baseline (main with these tasks), C1 and C2, `learn`, five repeats per
  task, nemotron-3-super, `EVAL_ISOLATE=1`, `EVAL_TIMEOUT=1800`, one job per
  key.
- Regression checks on all three builds: `stale-anchor` and
  `poisoned-lesson`, three repeats.

Reading, decided now:

- Stale use is counted as in the gone-anchor test: the old seed in any
  command that runs the suite, or written into the deliverable, once per run.
- The baseline offers nothing, so its stale count on
  `memory-changed-obsolete` is expected to be 0 of 5, and a candidate must
  match it: one stale run under a candidate is a fail, not a tie. If the
  baseline shows stale runs, the bar is the baseline's count.
- A candidate passes `memory-changed-fact` only if it scores more passes
  than the baseline and at least three of its scored runs, and at least
  three in five of them, pass with the lesson offered (with its warning) and
  the remembered seed used.
- On `memory-changed-obsolete-user`, C2 must not have more stale runs than
  the baseline; if it does, C2 fails, and its wording is not adopted for
  agent lessons alone. C1 does not change this path, so its runs there are
  recorded as a second baseline measurement, not as a bar.
- The regression checks pass unless a candidate scores fewer passes than
  the baseline on a task and a failing run was offered a lesson with the
  changed-file warning; a miss without that warning is read as the task's own
  rate.
- A candidate is merged only if it passes every bar that applies to it. If
  both do, the one with fewer stale runs on `memory-changed-obsolete` is
  merged (which matters only if the baseline shows stale runs and the bar
  rises above zero), and on a tie C1, as the smaller change. If neither does, both are parked, the result
  recorded, and main left as it is; any other wording or rule is a new
  predeclared test.
- Leak and infrastructure runs are reported and not scored. Any cell, of
  any build and task alike, with fewer than three scored runs gets further
  repeats under the same settings until it has three, at most ten runs per
  cell. There are no sampling seeds to fix: further repeats are new runs
  with the same model and settings, run as their own job after the first
  five, and every run is recorded and reported, including the ones not
  scored.

**Outcome: C1 and C2 parked; main unchanged.** The baseline 318f0c7 ran on
key 1 (04:56 to 05:27 UTC). C1 63946c0 ran on key 2 (04:56 to 05:11). C2
1534739 ran its tasks on key 1 (05:27 to 05:33) and its regression checks on
key 2 (05:11 to 05:31). Every run was scored, with no leak, timeout or
infrastructure run, so no cell was rerun. Each cell is passes, then stale
runs, then the request at which the lesson was first offered:

| | baseline | C1 | C2 |
|---|---|---|---|
| `memory-changed-fact` | 5/5; requests 5 to 32 | 5/5; request 0 | 5/5; request 0 |
| `memory-changed-obsolete` | 5/5, stale 0/5; requests 6 to 8 | 5/5, stale 1/5; request 0 | 5/5, stale 0/5; request 0 |
| `memory-changed-obsolete-user` | 5/5, stale 2/5; request 0 | 5/5, stale 1/5; request 0 | 5/5, stale 0/5; request 0 |
| `stale-anchor` | 3/3 | 3/3 | 3/3 |
| `poisoned-lesson` | 3/3 | 2/3 | 2/3 |

The premise was wrong. The baseline offered the agent lesson in every
changed-anchor run, with the warning, not at the first request but after the
session ran the suite and the `STATS_SEED is missing or wrong` error entered
the query, since `learn` observes tool calls and error lines. The check
before the runs asked with the request alone. The open item is a delay, not a
lost lesson.

Read as decided:

- `memory-changed-fact`: neither candidate can beat the baseline's 5/5, so
  both fail this bar.
- `memory-changed-obsolete`: C1 has one stale run against the baseline's
  none and fails. C2 has none and passes.
- `memory-changed-obsolete-user`: C2's 0/5 is not above the baseline's 2/5,
  so it passes. C1's 1/5 is recorded as a second measurement of the same
  path.
- Regression checks: every `poisoned-lesson` run, in every build, was
  offered only the seeded lesson. It has no anchored file and never carries
  the changed-file warning, so the misses are the task's own rate. Both
  candidates pass.
- C1 fails two bars and C2 one. Both are parked.

Reported, not decided on:

- **Time.** The late offer cost time. Two of the baseline's five
  `memory-changed-fact` runs took 628 s and 241 s (52 and 34 requests). Every
  candidate run of that task took 15 to 39 s.
- **Run-to-run spread.** On the user task the baseline and C1 run the same
  code and had two and one stale runs of five. C2's wording had none there
  and none on the agent task.

A wording-only change (C2's warning without C1's rule) or a bar on time to
the lesson would each be a new predeclared test. Such a test needs more than
five repeats per cell, since zero to two stale runs of five is within the
spread seen here.

### Predeclared: the changed-file warning's wording (2026-10-14)

The changed-anchor test above left one signal it did not decide on. With the
current warning (`Changed since: ONBOARDING.md; re-check before relying on
it.`), a lesson the user saved led to a stale suite run in three of ten
runs of `memory-changed-obsolete-user`: two on the baseline, one on C1, which
runs the same code there. With C2's wording there were none in ten across that
task and `memory-changed-obsolete`. W (63ac28d on 4bc026f) is C2's wording
alone: `Changed since: ONBOARDING.md; read the current version first and
follow it where it differs from this lesson.` Which lessons are offered, and
when, is unchanged.

Checked before any run, against the advisor with the tasks' lesson and
request: W and the baseline offer the same lessons for the request alone (the
user lesson after either reset; the agent lesson after neither), and only the
warning text differs.

Runs, written before the first repeat:

- Baseline (4bc026f) and W, `learn`, nemotron-3-super, `EVAL_ISOLATE=1`,
  `EVAL_TIMEOUT=1800`, one job per key, the two builds at the same time.
- Ten repeats each of `memory-changed-obsolete-user`,
  `memory-changed-obsolete` and `memory-changed-fact`; three each of
  `stale-anchor` and `poisoned-lesson`.

Reading, decided now:

- Stale use is counted as before: the old seed in any command that runs the
  suite, or written into the deliverable, once per run.
- `memory-changed-obsolete-user` decides. W must have at least two fewer
  stale runs than the baseline, out of ten each, since zero to two of five
  was the spread on identical code. If the baseline has fewer than two stale
  runs, there is no harm here for a wording to remove, and W is parked.
- `memory-changed-obsolete`: W must not have more stale runs than the
  baseline.
- `memory-changed-fact`: W must not have fewer passes than the baseline. The
  file there no longer gives the seed, and a stronger warning must not talk
  the model out of a lesson that still holds.
- Regression checks: W fails only if it scores fewer passes than the
  baseline on a task and a failing run was offered a lesson whose anchored
  file had changed, the only lessons the wording reaches.
- W is merged only if it passes all four; otherwise it is parked and main is
  left as it is. Two fewer stale runs of ten is a threshold fixed here, not a
  significance test: three of ten against one of ten is within chance, given
  the spread on identical code. A pass is recorded as a direction worth
  taking for a low-risk wording change, not as a measured effect.
- Reported for every run of the three tasks, not decided on: the number of
  requests to the first suite run with the correct seed, and the run's wall
  time (the trajectory has no clock per step), so the cost of reading the
  file first shows where the lesson was right.
- Leak and infrastructure runs are reported and not scored. Any cell, of
  either build, with fewer than seven scored runs gets further repeats, run as
  their own job after the first ten, until it has seven, at most fifteen runs
  per cell. Every run is recorded and reported.

**Outcome: W parked; main unchanged.** The baseline 4bc026f (key 1, 06:29
to 07:24 UTC) and W 63ac28d (key 2, 06:29 to 07:42) ran at the same time.
One W run was a leak: `memory-changed-fact` rep 7 read its own session's
`trajectory.json`, a harness record. It is reported and not scored. Every
other run was scored, and no cell fell below seven scored runs. Each cell is
passes of scored runs, stale runs, the median requests to the first suite
run with the correct seed, and the median wall time:

| | baseline | W |
|---|---|---|
| `memory-changed-obsolete-user` | 10/10, stale 5/10; 7; 29 s | 10/10, stale 1/10; 7; 25 s |
| `memory-changed-obsolete` | 10/10, stale 0/10; 6; 26 s | 10/10, stale 0/10; 6.5; 23 s |
| `memory-changed-fact` | 9/10; 7.5; 74 s | 7/9; 8; 101 s |
| `stale-anchor` | 3/3 | 3/3 |
| `poisoned-lesson` | 1/3 | 1/3 |

Read as decided:

- `memory-changed-obsolete-user`: the baseline has five stale runs, so
  there is harm to remove, and W has one, four fewer. It passes, recorded as
  a direction, not a measured effect.
- `memory-changed-obsolete`: none in either build. W passes.
- `memory-changed-fact`: W has seven passes against the baseline's nine,
  so it fails.
- Regression checks: equal passes on both tasks. W passes.
- W fails one bar and is parked.

Reported, not decided on:

- **The `memory-changed-fact` misses did not come from the wording.** W's
  two failed runs (reps 6 and 9) were never offered the lesson, so they never
  saw the warning. On runs where the lesson was offered, both builds passed
  every scored run (baseline 8/8, W 7/7). The gap is in whether the agent
  lesson is offered late at all, which is the same code in both builds. The
  bar counted passes, as decided, so it stands.
- **Seed guard.** One baseline pass on `memory-changed-fact` (rep 1) came
  from editing the seed guard (#101). It counts as recorded; without it W is
  still below the baseline, seven to eight. No run of
  `memory-changed-obsolete-user`, in either build, touched the guard.
- **Cost.** Requests to the first correct suite run were within one of
  each other on every task. Wall time on `memory-changed-fact` ranged from 23
  to 908 s, driven by how late the lesson was offered, not by the build.

Any later test of this wording would need a `memory-changed-fact` bar that
counts only runs shown the warning, as the regression rule does. That would
be a new predeclared test.

### Seed tasks: a new cohort (2026-10-15)

The six seed tasks (`memory-only-fact`, `memory-anchored-fact`,
`memory-obsolete-fact` and the three `memory-changed-*`) now write
`sha256("suite_ran:" + seed)` to `.suite_ran`, and each check compares
against that value. Editing the guard, rebinding its hash on a later line, or
writing the marker by hand no longer passes; only a run that knows the seed
does. #101 caught one of these by the guard's text; this closes the rest. The
file the agent sees changed, so runs from here on are a new cohort, and are
not pooled with the runs above.

### Predeclared: the changed-file wording, retested (2026-10-15)

The wording test above parked W on its `memory-changed-fact` bar, which
counted all passes. Both of W's misses there were runs never offered the
lesson, so they never saw the wording; the bar could not tell a wording harm
from late delivery, which is the same code in both builds: all-run passes
were 9/10 against 7/9, while runs offered the lesson passed 8/8 and 7/7. This
retest changes that bar, after seeing #102's data, which is a forking path, and it
is stated as one. The deciding bar is unchanged, the runs are new, and
#102's runs are not pooled with them.

- Baseline e7f0a8f (main, with the seed-derived marker of the new cohort)
  and W 27fe97b (the same wording change rebased on it), `learn`,
  nemotron-3-super, `EVAL_ISOLATE=1`, `EVAL_TIMEOUT=1800`, one job per key,
  the two builds at the same time.
- Ten repeats each of `memory-changed-obsolete-user`,
  `memory-changed-obsolete` and `memory-changed-fact`; three each of
  `stale-anchor` and `poisoned-lesson`.

Reading, decided now:

- Stale use is counted as before.
- `memory-changed-obsolete-user` decides, unchanged: W needs at least two
  fewer stale runs of ten than the baseline, and is parked if the baseline
  has fewer than two. A pass is a direction for a low-risk wording change,
  not a measured effect.
- `memory-changed-obsolete`: W has no more stale runs than the baseline.
- `memory-changed-fact`: W's failures as a share of its offered, scored
  runs are no higher than the baseline's. A share, not a count, since the two
  builds can be offered the lesson a different number of times; at these
  sizes it is still a threshold, not a test. All-run passes are reported, not
  decided on.
  A run was offered the lesson when, before its first suite run with the
  correct seed (or at any point, if it has none), the lesson's id is in its
  trajectory's `memory.offered` at that request or an earlier one, or a
  `recall` result before it holds the lesson's text. This rule is applied to the trajectory as it
  stands; no run is classed by hand.
- That bar needs at least five offered, scored runs per build. A build with
  fewer gets further repeats of that task, up to fifteen runs in the cell; if
  either build still has fewer than five, the bar is undecided and W stays
  parked.
- Regression checks as in the wording test above.
- W is merged only if it passes all four; otherwise it is parked and main is
  left as it is.
- Leak and infrastructure runs are reported and not scored. Any cell with
  fewer than seven scored runs gets further repeats, for both builds alike,
  up to fifteen per cell. Every run is reported, with requests to the first
  correct suite run and wall time.

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
`EVAL_ISOLATE=1` (Linux, as root) runs each session in its own mount and pid
namespace. The session then sees only its own working copy and home in the
temp dir, sees what the home held before it started (memory store, logs,
earlier trajectories and saved conversations) as empty, and cannot see the
task sources, earlier results, the paths in `EVAL_HIDE` or other processes.
Without it, concurrent runs share the temp dir: a session that searches the
filesystem can read another run's working copy or its own memory store, and
the leak verdict only reports that afterwards.
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
The rule is versioned, and each run prints its version. Rule 2 treats part of
the run's own working-copy name (the `rusty-eval-` stem and at least one
digit), such as a session typing it as a guess, as its own, not as an outside
read, unless a directory of exactly that name sits beside the working copy.
Runs scored under rule 1, among them the memory-use runs at 14fba44 and
ae99853, keep their verdicts.
A task can also list, in `leak.txt`, strings a session should get only from
memory. Each session that starts without one in its working copy is checked.
If it meets one in a tool result before using it itself, that is a `leak`
too: the string came from another run's files, `/tmp` or the harness.
Results from `recall` are memory and don't count.

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
  no lessons; builds after the reporting pin also offer no memory tools
  (`remember`, `recall`, `forget`) with it, where the pin offered them and
  each call answered "memory is off". `--agents off` runs one agent. Any
  other value is a different experiment.
- **Compaction.** Builds after the reporting pin accept `--compaction`
  (`aggressive`, `standard`, `passive` or `smart`; listed by
  `--capabilities`). It falls back to the saved setting, so a run on such a
  build should pin it, normally to `--compaction standard`.
- **Context window.** Builds after the reporting pin measure context fill
  from the provider's reported prompt tokens and look up the window: from
  `--context-tokens`, else the endpoint's model list (one GET at startup),
  else saved settings, else 128k. Pin `--context-tokens` to keep that GET
  off the gateway and the window fixed. The reporting pin assumes 128k and
  estimates fill from characters.
- **`--verify`** is optional. Without it, the goal closes on the model's word,
  and exit status 2 means only that the goal stalled.
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
| 2 | the goal did not close: with `--verify`, not on a passing check; with or without it, after stalling three turns in a row (contract 2 and later; contract 1 exited 1 on a stall) |
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
  - `goal_end` (contract 2 and later, top-level here and in the
    trajectory): how the goal run ended, `done`, `blocked`, `turn_limit`,
    `stalled`, `interrupted`, `paused` or `error`. It is null when no goal
    ran, and absent on builds before contract 2;
  - `claims`: what happened to the model's completion claims (see above);
  - `safety`: consequential calls proposed, executed and blocked (see above);
  - `execution_mode`, `memory_mode`, `tools_location`;
  - `compaction` and `context` (builds after the reporting pin): window,
    `window_source`, `calibration`, how many requests carried provider
    usage, and cache reads and writes. The trajectory also lists each
    request's `usage`.
  - `compactions` (builds after the reporting pin): one entry per
    compaction with its `trigger` (`level`, `smart`, `manual` or
    `overflow`), `tokens_before` and `tokens_after` as the meter measures
    them, `calls_since_last`, `boundary`, the index in the trajectory's
    `messages` of the summary that replaced history, `resumes_at`, the
    index of the first message after the compaction, `reverted`, how many
    files it found with their edits undone, and `model`, the model that
    wrote the summary (`context` names the configured one as
    `compaction_model`). The messages from `boundary` up to `resumes_at`
    are the summary and those kept verbatim, which ran before it. A
    report reads what followed each compaction (repeated calls, a
    first-step error, files read again) from `resumes_at` on.
  - `memory` (null when off). Builds after the reporting pin add
    `offered`: each lesson offered, with its id, its text and the index in
    the trajectory's `usage` of the first request that carried it.
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
