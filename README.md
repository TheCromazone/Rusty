<div align="center">

```
   ⋆        ·      ✦                 ·    ⋆               ✧        ·
          ▀▀▀▀▀▀▀▀▀▀▀   ▀▀▀      ▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀  ▀▀▀▀▀▀▀▀▀▀▀▀  ▀▀▀      ▀▀▀
         ▀▀▀▀▀▀▀▀▀▀▀▀  ▀▀▀      ▀▀▀  ▀▀▀▀▀▀▀▀▀▀▀▀  ▀▀▀▀▀▀▀▀▀▀▀▀   ▀▀▀    ▀▀▀
        ▀▀▀      ▀▀▀  ▀▀▀      ▀▀▀  ▀▀▀               ▀▀▀▀        ▀▀▀  ▀▀▀
       ▀▀▀      ▀▀▀  ▀▀▀      ▀▀▀  ▀▀▀▀▀▀▀▀▀▀▀       ▀▀▀▀         ▀▀▀▀▀▀
      ▀▀▀▀▀▀▀▀▀▀▀   ▀▀▀      ▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀      ▀▀▀▀          ▀▀▀▀
     ▀▀▀▀▀▀▀▀▀▀    ▀▀▀      ▀▀▀           ▀▀▀      ▀▀▀▀          ▀▀▀▀
    ▀▀▀    ▀▀▀    ▀▀▀      ▀▀▀           ▀▀▀      ▀▀▀▀          ▀▀▀▀
   ▀▀▀     ▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀▀  ▀▀▀▀▀▀▀▀▀▀▀▀      ▀▀▀▀          ▀▀▀▀
  ▀▀▀      ▀▀▀   ▀▀▀▀▀▀▀▀▀▀   ▀▀▀▀▀▀▀▀▀▀▀       ▀▀▀▀          ▀▀▀▀
     ·         ✦ ⋆        ·  ✦        ⋆ ·              ⋆          ✧
```

**A coding agent for your terminal. Written in Rust.**

*All the good parts of modern coding agents. None of the stuff that wears you down.*

![rust](https://img.shields.io/badge/rust-2021-b7410e?logo=rust)
![binary](https://img.shields.io/badge/one%20static%20binary-no%20runtime-a0a8b2)
![evals](https://img.shields.io/badge/evals-10%E2%80%9312%20of%2012%20by%20mode-ec7a34)
![tests](https://img.shields.io/badge/tests-130%20passing-ec7a34)
![models](https://img.shields.io/badge/models-NVIDIA%20%C2%B7%20Anthropic%20%C2%B7%20OpenAI%20%C2%B7%20any%20compatible-6c757d)
![license](https://img.shields.io/badge/license-MIT-6c757d)

<img src="docs/demo/hero.gif" alt="rusty finds and fixes two bugs in a small shop app, then runs the tests" width="100%">

<sub>A real session (idle pauses trimmed, played back a little faster). rusty finds two real bugs in a discount module, fixes both, and proves it by running the tests.</sub>

</div>

---

## Why rusty

Coding agents are great for the first twenty minutes of a task and shaky for
the next three hours. They forget what you said once the context fills up.
They announce "done" without running anything. They ask before `ls`, then wave
through `rm -rf`. Their memory turns into a junk drawer pasted into every
prompt. And you find out what they cost when the bill arrives.

rusty is built around fixing exactly those problems. It's one static binary
with no telemetry. It runs on open models hosted by NVIDIA by default, talks
to Claude and GPT models directly, and works with any OpenAI-compatible
endpoint.

## See it work

Every clip below is a real session, recorded with
`cargo xtask record` ([`xtask/src/record.rs`](xtask/src/record.rs)) against the toy store in
[`demo/shop`](demo/shop) (or against rusty's own source). The only edits are trimmed idle pauses and
faster playback. Run them yourself and you'll get the same kind of result.

### A swarm, when one pair of eyes isn't enough

<img src="docs/demo/swarm.gif" alt="four parallel workers audit four modules and return a ranked bug list" width="100%">

`/agents auto`, then one sentence. rusty starts four read-only workers, one per
module, each with its own context. It shows each one finishing, then merges
their reports into a ranked list. The two real bugs come out on top.
<sub>(Sped up 2×.)</sub>

### `/goal`: keep going until it's actually done

<img src="docs/demo/goal.gif" alt="goal mode plans, writes a CLI and its tests, runs them, and closes with evidence" width="100%">

Goal mode plans, writes `shop/cli.py` and its tests, and runs them. It also
checks the edge cases by hand. It can only finish by calling `goal_done` with
evidence, so "I think it works" doesn't count.

### `/compact`: a summary that doesn't forget

<img src="docs/demo/compact.gif" alt="compaction shrinks 12.6k tokens to 864 and the next answer still knows the details" width="100%">

rusty reads two of its own source files (12.6k tokens). `/compact keep how the
working set is built` shrinks that to **864 tokens** and saves three lasting
facts to memory. The follow-up question is answered correctly **without
reading a single file again**. Your messages and a list of files changed and
commands run are carried over verbatim, so the summary can't lose them.

### Switch models mid-session for a second opinion

<img src="docs/demo/models.gif" alt="doctor checks each provider, Nemotron Super fixes the discount bug, then Nemotron Ultra reviews the fix" width="100%">

`/doctor` checks every provider it has a key for and says where the current
model goes. Nemotron Super finds and fixes the discount bug and runs the tests.
Then `/model nvidia/nemotron-3-ultra-550b-a55b` switches mid-session, and the
bigger model reviews the fix with the whole conversation in view. Claude and
GPT models work the same way: rusty picks the provider from the model id on
each request.
<sub>(Sped up 2×.)</sub>

### Focus mode, memory, permissions and token honesty

<img src="docs/demo/focus.gif" alt="adhd view gives a three-bullet answer; memory, permission check and token breakdown" width="100%">

`/view adhd` gets you one status line and an answer in three bullets.
`/remember` saves a lesson for future sessions. `/permissions check` shows the
classifier at work: `rm -rf` asks first while `cargo test` just runs.
`/tokens` shows where every token went.

## Everything else

| | |
|---|---|
| **High-recall context** | Budgets scale with the model's real context window, and fullness comes from the provider's own token counts. Old tool output gets trimmed first, because it can be re-read, and only then are older turns summarised. Your messages, the plan and the goal survive every compaction, and a file whose edits were undone is listed as unchanged rather than changed. |
| **Real memory** | Typed lessons (`preference`, `fact`, `decision`, `gotcha`) offered one at a time when BM25 relevance × confidence × file freshness clears a bar, not a file pasted into every prompt. Lessons are tied to the files they describe and gain or lose confidence only when a goal's fixed check passes or fails. `--memory off · recall · learn · reflect · deep` sets how aggressively memory captures, applies and prunes ([docs/MEMORY.md](docs/MEMORY.md)). Benchmark runs should pass `--memory off`. |
| **Permission classifier** | Builds, installs, feature-branch pushes and read-only cloud commands just run. Pushes to main, deploys and anything unclear ask. Force pushes, cloud and database deletes and `terraform destroy` need a typed yes every time, even in yolo. |
| **Execution modes** | `careful` has a read-only checker review finished work, `vibe` moves fast with a few read-only workers, `standard` sits in between. Separate from permissions. |
| **Infrastructure harness** | Knows where it's pointed: the banner and `/target` show the kube context, cloud account, Terraform workspace and branch, and a production target makes auto pick careful. In careful mode there's no apply without a diff or plan from the same session, a snapshot of what a change touches is taken before it runs, and the rollout is checked after. Every infra command goes into an audit log, and `/changes` shows what the session changed. See [docs/INFRA.md](docs/INFRA.md). |
| **Secrets stay out of the transcript, and still work** | Tool output is redacted before the model, the screen or a session file sees it. Each secret shows up as a name like `${RUSTY_SECRET_1}` that your shell commands can use, so the agent can connect to a database with a password it never reads. |
| **`/loop`** | `/loop 5m check CI and fix failures`, or let rusty set its own pace. |
| **Subagents and swarms** | `/agents sub\|swarm\|auto`. A swarm takes up to 32 tasks and runs them through a pool, and workers share findings with each other while they work, so they build on each other instead of repeating searches. Workers are read-only; the lead confirms what they found and makes every change. Models can rotate and temperatures spread. Off by default, so you never pay for tokens you didn't ask for. |
| **Skills** | `/review` `/explore` `/feature` `/debug` `/test` `/commit`, plus your own markdown skills per project or per user. |
| **MCP servers** | Put stdio servers in `.mcp.json` (`{"mcpServers": {...}}`, or point `RUSTY_MCP_CONFIG` at one) and their tools, every page of them, appear as `mcp__server__tool`; a name that is too long or clashes ends in a stable hash. Reads run freely, deletes (by name or `destructiveHint`) need a typed yes, everything else follows your permission mode. A broken or `url` server is skipped with a warning unless it has `"required": true` or `"requiredTools": [...]`, which stop startup. `rusty --mcp-check` prints what connected as JSON, with a `reason` for each gap (`unsupported_transport`, `bad_config`, `start_failed`, `handshake_failed`, `discovery_failed`, `missing_tools`), and exits 1 if something required is missing. |
| **Terminal-native tools** | `search` (ripgrep), `glob`, `outline` (definitions with line numbers), ranged `read_file`, exact `edit_file`, `bash`. Find first, then read only what you need. An edit that would break a file that parsed before is refused, with the line and the error: a JSON file always, a Python or shell file when its parser is installed. |
| **Knows where it is and what's left** | The first request carries a snapshot of the workspace (files, git state, the CLIs on PATH), so no calls go on `ls` and `which`. In a bounded run every request says how many calls, tokens and minutes are left, and near the end it says to finish rather than explore. Goals close only with evidence for each requirement. |
| **Token tips** | After a turn that spent heavily, one specific tip based on what actually happened, never generic nagging. |
| **Instant Ctrl-C** | Stops a turn mid-stream in about 15 ms, and the next turn works. Type while it works and your message runs next. Press Ctrl-C twice to quit. |
| **Looks good** | Rust-and-steel themes, a starfield banner with today's mission line, a steady geometric status panel with falling diamonds, grouped tool lines, rendered markdown. |

## Quickstart

```bash
git clone https://github.com/jadenfix/rusty && cd rusty
cp .env.example .env        # add NVIDIA_API_KEY, ANTHROPIC_API_KEY or OPENAI_API_KEY
cargo install --path .
```

```bash
rusty                                         # interactive
rusty "why does the build fail?"              # one shot
rusty --goal "get cargo test green" --verify "cargo test" --yolo
rusty -c                                      # resume the last session here
rusty -c 3f9a12c0                             # resume one by id (a prefix is enough)
rusty --sessions [words]                      # list the sessions saved here, or search them
rusty --doctor                                # which providers are set up and answering
rusty --capabilities                          # accepted values as JSON, for harnesses
rusty --toolset shell "fix the build"         # offer only bash, for interface experiments
```

Keys come from your shell, then `~/.config/rusty/.env`, then the `.env` next
to rusty's `Cargo.toml`. Set any of `NVIDIA_API_KEY`, `ANTHROPIC_API_KEY` and
`OPENAI_API_KEY`; with several keys for one provider (`ANTHROPIC_API_KEY_2`,
…), rusty rotates through them and backs off politely when rate limited:
it keeps retrying rate limits and server errors for up to five minutes
(`RUSTY_RETRY_SECS`), with jitter so parallel agents don't retry in step, and
gives up quickly on a key that is refused.

## Commands

`--goal ... --verify 'command'` pins a local acceptance command chosen by you.
Rusty runs it after the completion proposal's remaining tools and any careful
review. A failure, timeout, interruption, denied permission, or source-input
change keeps the goal open. Use `--verify-timeout 120` to set its deadline
(1–600 seconds). An unfinished verified goal exits with code 2; Ctrl-C remains
130. Goals without `--verify` retain their existing completion policy.

The command still obeys permissions. It establishes that the chosen check ran
successfully, not that it covers every requirement. For criterion integrity,
use inline assertions or a checker outside the editable workspace. Records in
sessions and `--trajectory` include observed status and workspace hashes;
resuming reruns verification rather than trusting saved receipts. See
[docs/VERIFICATION.md](docs/VERIFICATION.md) for input exclusions, scope, and
the staged repair/search roadmap.

Type `/` and a menu of commands opens under the prompt, narrowing as you type
(`/cmpct` still finds `/compact`). ↑↓ choose, Tab or → completes, and Enter
runs the highlighted command, or fills it in when it needs an argument. After a
command and a space the menu offers its values, such as `/mode ` → `careful`.
Piped input runs a partial command only when one command matches, and an
unknown one gets a "did you mean".

| | |
|---|---|
| `/goal <objective>` | autonomous until done and verified · `status` · `resume` · `clear` |
| `/loop [5m] [x10] <prompt>` | repeat on an interval, or self-paced |
| `/agents off\|sub\|swarm\|auto` | delegation · `/agents model <id>` sets the subagent model |
| `/swarm size\|models\|spread` | workers at once (up to 16; a swarm call takes up to 32 tasks and workers share findings), model rotation, temperature spread |
| `/compact [focus]` | summarise now, keeping what you name in detail |
| `/resume [id\|words]` | list saved sessions, search them, or pick one up |
| `/compact settings [mode]` | when to compact: `aggressive` (summarise at 60% of the usable window), `standard` (80%, the default), `passive` (95%) or `smart` (the model compacts when a sub-task is done; rusty refuses at a bad moment and keeps its last two steps verbatim) |
| `/target` · `/audit [n]` · `/changes` | where infra commands will land · the infra audit log · what this session changed |
| `/triage` `/change` `/postmortem` | infra skills: incident triage, a change plan with rollback, a blameless write-up |
| `/context` · `/tokens` | where the window is going · spend by model and by tool |
| `/memory` · `/remember` · `/forget` | long-term memory |
| `/permissions [mode]` | `read-only` `ask` `auto` `yolo` · `allow` · `deny` · `check <cmd>` |
| `/mode auto\|careful\|standard\|vibe` | how hard rusty thinks and checks its work |
| `/tools [local\|daytona]` | show tool placement; save the default for the next session |
| `/view default\|verbose\|adhd` | how much you see |
| `/theme` · `/font` | `rust` `neon` `matrix` `amber` `ice` `mono` · `rust` `block` `thin` `classic` |
| `/review` `/explore` `/feature` `/debug` `/test` `/commit` | built-in skills · `/skills` lists yours too |
| `/doctor` | each provider's keys (names only), endpoint, model count and latency, and where the current model goes |
| `/model` · `/models` · `/plan` · `/tips` · `/settings` · `/clear` · `/exit` | |

rusty reads the context window from the endpoint's model list. When the
endpoint doesn't publish one, pass `--context-tokens`, or rusty assumes 128k
and says so. `--history-cap-tokens` makes compaction start by that many tokens
however large the window; above a 200k window the cap is 128k unless you set
one. `--compaction` sets the mode for one run, and `--compaction-model` hands
summaries to another model, with the session's model as the fallback.
`--stats` and `--trajectory` record each compaction: why it ran, the tokens
before and after, and where the work resumed.

## Execution modes

How hard should rusty think, and how many times should it check its work?
That's separate from what it's allowed to do.

| | |
|---|---|
| `auto` | The default. rusty picks one of the three below for each request and says why on the first line: careful when it mentions production, a migration, a deploy, data, money, security, infrastructure or an incident; vibe for a quick prototype or sketch; standard otherwise, including whenever it's unsure. During a turn it only ever moves up to careful, when a consequential command comes up or three commands fail in a row. It never changes permissions or the model. |
| `careful` | Thinks longer. When it says it's done, a separate read-only checker looks at the real files and your instructions and reports what's wrong. rusty fixes that and verifies it, with no second review. A plain question isn't checked, and a goal gets one checker attempt, including after saving and resuming. |
| `standard` | Normal effort, and it checks what it changed. |
| `vibe` | Quick passes and small checks. It can send out up to three read-only workers to look things up in parallel. It still has to run a relevant check before it calls something done. |

```bash
rusty --mode careful --goal "move the sessions table to the new schema without losing rows"
rusty --mode vibe "sketch a settings page"
```

Switch with `/mode auto|careful|standard|vibe`; the choice is saved, and an
explicit mode always beats auto. `--mode` or `RUSTY_MODE` sets it for one
run. To use a different model per mode, set
`RUSTY_CAREFUL_MODEL`, `RUSTY_STANDARD_MODEL` or `RUSTY_VIBE_MODEL`
(`--model` and `/model` still win). If you choose `/agents` settings
yourself, they stick; `/agents default` hands delegation back to the mode.

On the default NVIDIA model, careful and vibe also set the model's
reasoning budget. In a quick test, the low budget clearly cut reasoning,
but asking for "high" made no difference on easy questions. Whether careful
mode beats standard on real tasks is still being measured; see the evals
below.

All profiles stop a turn after four consecutive identical tool calls return
unchanged results. Remaining tools in that batch are skipped and the goal stays
open. A different call or changed output resets the counter; JSON formatting
changes do not. This bounds exact-call loops, not every possible lack of progress.
It adds no model request, checker or dependency.

## Permissions

Built for letting an agent run on its own, with a hard stop for anything
you can't take back.

```
                 reads, searches,   project edits,     pushes to main,    force push, cloud or
                 cloud reads        builds, installs,  deploys, PRs,      database deletes,
                                    feature pushes     anything unclear   rm -rf outside, disks
read-only            run               refused            refused            refused
ask                  run               asks               asks               needs you, every time
auto                 run               run                asks               needs you, every time  ← default
yolo                 run               run                run                needs you, every time
```

- **Runs on its own in auto:**
  - building, testing, linting and installing project dependencies;
  - deleting build folders (`rm -rf node_modules dist target`);
  - everyday git (commit, branch, rebase, pull, push a feature branch);
  - calls to your local dev server, and plain downloads;
  - read-only cloud commands: `aws … describe/list/get`, `kubectl get/logs/describe`, `terraform plan`, `gcloud … list`, `gh pr view`, `docker ps`, `SELECT` queries.
- **Asks first:**
  - pushing to `main` or another shared branch;
  - `git reset --hard` and anything else that throws away uncommitted work;
  - deploys, `terraform apply`, `kubectl apply`;
  - opening or merging pull requests;
  - writes outside the project;
  - reading `~/.aws/credentials` and other keys;
  - sending local data off the machine;
  - commands rusty doesn't recognise.
- **Needs a typed `yes` every time, even in yolo:**
  - force pushes and deleting remote branches or history;
  - `terraform destroy`;
  - cloud deletes (`aws s3 rm`, `kubectl delete`, `gcloud … delete`, `fly apps destroy`);
  - `DROP`, `TRUNCATE`, and `DELETE` without `WHERE`;
  - `prisma migrate reset`;
  - `rm -rf` outside the project;
  - disk writes.

  An allow rule can't skip this. With nobody at the keyboard (a script, CI, a benchmark), it's refused, and the model is told to hand the command to a person.
  The one exception is a disposable sandbox that a harness runs on purpose: `RUSTY_ALLOW_DESTRUCTIVE=1` together with `--yolo` and no terminal lets these run, each with a warning line. It has no effect in an interactive session or any other permission mode.

rusty looks at what a command will really run, not just its first word:
- the scripts it calls, `package.json` scripts, `make` targets;
- heredocs piped into a shell, and `$(…)` substitutions;
- commands sent through `ssh`, `docker exec` and `kubectl exec`;
- code passed to `python -c`.

So `npm run deploy` is judged by what `deploy` does.

`/permissions check <command>` shows the verdict and why. Deny rules always
win. At a prompt, `a` allows that kind of call from then on, and typing
anything else sends it back to the model as feedback.

## Verified end to end

The live rows use the real model and a real terminal, and every eval check
is hidden from the agent. The offline tests drive the real binary against a
scripted stand-in for the model, so they're free and repeatable.

| What | Result |
|---|---|
| `cargo xtask qa`: fmt, clippy `-D warnings`, 130 offline Rust tests (including the real binary and memory service), 6 memory smoke cases, 7 cloud launcher tests | pass |
| Live end-to-end against the model: bug fix, goal mode, swarm, Ctrl-C exit code | 4/4 |
| Real pseudo-terminal: Ctrl-C during a streaming turn | stops in ~10-70 ms, next turn works, double Ctrl-C quits |
| 12 evals in each mode: Python, Rust and JS fixes, code search, a cross-file rename, a CLI flag, a change of plan midway, memory across sessions, a rule surviving compaction, undo one step, a long `/goal`, `/compact` with a focus | careful 12/12 · standard 11/12 · vibe 11/12 · auto 10/12 |
| Inside a FullStack-Bench task container under Harbor | installs, works the task with the environment's own CLI, closes its goal with evidence |

These are single runs of each task on the same model, so a difference of
one or two tasks between modes isn't meaningful yet. Runs that the API's
rate limit cut off were rerun, and every miss was read from its logs:
- standard and auto each went in circles on one task until the time limit;
- vibe closed one goal that failed the hidden checks;
- auto ran out of time on another goal even after switching itself up to careful.

Careful used about 2.5 times the tokens of standard on the tasks both passed.

Five newer tasks (a Rust ring buffer, exact-cents bill splitting, backup
rotation in bash, a bounded-concurrency `fetchAll` in JS, and a TTL cache
written from its docstring) check more than their visible tests do, so
fixing only the example doesn't pass. The `evals` workflow runs any of this
on demand from GitHub with the repository's keys.

```bash
cargo xtask qa           # offline checks
cargo xtask qa --live    # plus live tests, the terminal check and every eval
cargo xtask memory-smoke --live --binary target/release/rusty   # the memory cases at every level on NVIDIA
cargo xtask memory-bench # memory advice latency with 512 stored lessons (release build)
cargo xtask eval multi   # just the multi-turn evals; logs land in target/evals/

# the same tasks on several models, three runs each, compared in one table
EVAL_MODELS="claude-opus-5-5 gpt-5 nvidia/nemotron-3-super-120b-a12b" EVAL_REPEATS=3 cargo xtask eval
# the same, comparing memory levels side by side
EVAL_MEMORY="off learn reflect" EVAL_REPEATS=3 cargo xtask eval multi
cargo xtask eval-report target/evals/*.jsonl   # pool earlier runs into one table
```

## Models

Anything your endpoint serves, chosen with `--model`, `RUSTY_MODEL` or `/model`.

| Model on NVIDIA's API | |
|---|---|
| `nvidia/nemotron-3-super-120b-a12b` | **default**, fast and dependable with tools (every demo above) |
| `nvidia/nemotron-3-ultra-550b-a55b` | bigger, slower, thinks harder |
| `z-ai/glm-5.3` | strong coder, slow to first token |
| `openai/gpt-oss-20b` | small and quick; a good swarm worker |

| Anthropic and OpenAI | |
|---|---|
| `claude-opus-5-5` | the default when `ANTHROPIC_API_KEY` is the only key; adaptive thinking, effort follows the execution mode |
| `claude-sonnet-5-5`, `claude-haiku-4-5` | faster and cheaper; Haiku makes a quick swarm worker |
| `gpt-5` | the default when `OPENAI_API_KEY` is the only key; reasoning effort follows the execution mode |

rusty picks the provider from the model id, per request, so one session can
mix them: `/model claude-opus-5-5` for the lead and
`/agents model openai/gpt-oss-20b` for the workers. Bare `claude-*` ids go to
Anthropic's Messages API, bare `gpt-*` and `o4`-style ids to OpenAI, and
anything else (including `openai/...` ids on NVIDIA) to the compatible
endpoint. `RUSTY_PROVIDER=anthropic|openai|compatible` overrides the guess.

On Claude, thinking blocks go back to the API unchanged between turns, and
since compaction rewrites history, rusty asks the API to drop a thinking
block whose conversation changed instead of failing the request. On the
models that support it, a request Claude's safety checks decline is retried
on Anthropic's recommended fallback model inside the same call.

To use another endpoint, set `RUSTY_BASE_URL`, for example `http://localhost:11434/v1`.
A `claude-*` or `gpt-*` model with no key of its own goes there too, so a local
router keeps working. `ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` point those
providers somewhere else.

## Run it anywhere

Keep reasoning and memory local while executing the entire swarm's tools in
Daytona. `rusty-cloud hybrid` talks to Daytona's REST API and attaches to an
already started sandbox; it never creates or wakes one. The snapshot must contain
this version of rusty and the remote workspace must already exist.

```bash
cargo build --bins
target/debug/rusty-cloud hybrid --sandbox <id> --workspace /home/daytona/work \
    --mode standard --agents swarm --swarm-max 3 --memory-mode learn \
    --prompt "Use three workers to inspect the project, then fix and verify the bug."
```

`--tools local|daytona` (or `RUSTY_TOOLS`) overrides the saved default.
`/tools` and `/settings` show the current location; `/tools local|daytona`
changes the next session, so a goal never changes workspaces halfway through.
The hybrid launcher supplies the ephemeral localhost tool bridge for
`--tools daytona`. Swarm workers remain read-only and share the lead's remote
checkout; only the lead edits. No model credentials are copied to the sandbox.
Cloud compute and model usage still consume credits. Full setup, lifecycle,
placement choices and evidence are in [docs/CLOUD.md](docs/CLOUD.md).

The core CLI is one static binary. `docker build --target bin -o out .` produces
`rusty` and the optional `rusty-memoryd` companion for Linux.
[docs/MEMORY.md](docs/MEMORY.md) covers the memory levels `--memory off|recall|learn|reflect|deep`, the
RAM-only L1, packed L2, learning from feedback, live checks and cloud transfer.
[docs/ARCHITECTURE_AUDIT.md](docs/ARCHITECTURE_AUDIT.md) records the storage,
dependency and privacy tradeoffs. Redistributed binaries include `LICENSE` and
`THIRD_PARTY_NOTICES.txt`.
[docs/CLOUD.md](docs/CLOUD.md) covers Daytona sandboxes
(`rusty-cloud run --repo … --goal …`), AWS, Devbox and Docker.
rusty also plugs into Harbor benchmarks with `--goal`, `--trajectory` and
`--stats`.

## Inside

```
  prompt · /goal · /loop
            │
  ┌─────────▼──────────── agent loop ───────────┐      ┌──────────────┐
  │ mode     auto ▸ careful · standard · vibe   │ ◀──▶ │ model, SSE   │
  │ context  budgets · elision · /compact       │      │ key rotation │
  │ memory   BM25 recall                        │      └──────────────┘
  └─────┬────────────────────────────────┬──────┘
        ▼                                ▼
  permissions ▸ infra ▸ tools        read-only workers
  runs · asks   target   bash        subagent · swarm
  · needs you   audit    files       careful checker
```

```
src/
  main.rs         CLI, REPL, slash commands, settings
  agent.rs        the loop: model ⇄ tools, goal, loop, subagents and swarms, compaction
  display.rs      grouped tool lines, status line, recaps, views
  llm.rs          streaming client on a background thread, tool-call assembly, key rotation
  tools.rs        read, write, edit, list, search, glob, outline, bash
  permissions.rs  the command classifier
  advisor.rs      memory levels and the advisor: lessons, confidence, file anchors, credit (rusty-memoryd)
  recall.rs       identifier-aware tokenizer and BM25 inverted index
  context.rs      budgets, elision, compaction, the working set
  skills.rs       built-in and user skills
  tips.rs         token tips from real usage
  markdown.rs     terminal markdown
  ui.rs           themes, banner, mission line
  footer.rs       steady status panel and falling geometric rail
  execution.rs    careful, standard and vibe profiles, and auto's pick
  infra.rs        live target, dry-run gate, snapshots, audit log, redaction
```

Why it's built this way: [docs/DESIGN.md](docs/DESIGN.md) covers where coding
agents hold up on long tasks, where they fall apart, and what rusty does about
each.

## Contributing

Conventional Commits, and pull requests written by a person, for people. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT

The status panel stays beneath the transcript on the normal terminal screen;
completed output enters ordinary scrollback. The three diamonds fall inside
the panel instead of moving the entire status line. Terminals narrower than
31 columns or shorter than eight rows use one compact line. `RUSTY_NO_ANIM=1`
keeps the rail still; `NO_COLOR=1` and pipes use plain output. No animation
thread or full-screen terminal framework is involved. Unicode cell measurement
uses the same `unicode-width` version already used by rustyline.

To capture and inspect the real CLI without model credentials:

```bash
cargo xtask tty-capture --binary target/debug/rusty --output /tmp/rusty-tty
cargo xtask tty-inspect /tmp/rusty-tty
```

The captures exercise real file writes, Bash results, streamed replies, and
interactive Ctrl-C recovery on a real pseudo-terminal at 24/40/80/120 columns,
against a local stand-in provider. The inspection replays each capture on a
small terminal screen model and checks every panel frame for complete borders,
an unused wrap column, colour, a footer that stays on the bottom row, and a
clean exit. `cargo xtask tty` runs the same checks as a quick pass/fail.
