# Native Mac app foundation

The Mac interface lives under `apps/macos` in this repository and will ship as
its own `.app`/DMG. The normal CLI has no Swift, web view, UI assets or GUI
framework dependencies. Rust remains the provider, tool and permission authority.
The current change is the engine foundation and a separate experimental diff
viewer, **not a finished desktop app**.

## Private workers

A native client starts one `rusty --session-worker` process per session, with a
private socketpair inherited as fd 3. `--inspect-worker` provides a separate,
read-only repository channel without loading dotenv, settings, provider clients
or memory services. `--dir` selects the project. Inspection runs separately so a
large Git request cannot delay session cancellation or approval decisions.

For a future SSH adapter, `--protocol-stdio` carries frames over stdin/stdout.
Protocol descriptors are close-on-exec; the original stdin becomes `/dev/null`
and stdout becomes diagnostics, so tools cannot consume or corrupt frames.
No listening TCP server or new authentication surface is created. The parent
must drain diagnostics continuously and retain only a bounded tail.

The wire format is newline-delimited JSON, capped at 4 MiB including newline.
An incomplete final frame, invalid UTF-8/JSON, or missing/incompatible protocol
version is fatal to the input stream. The writable session worker advertises
`recoverable_controls`: complete version-matching JSON with an unknown method,
invalid parameters/envelope, or invalid request ID receives a bounded
`reply{ok:false,error:"invalid control",code:...}` without changing the active
run, pending approval, or lease. Codes are `invalid_command` and
`invalid_request_id`; valid IDs are echoed exactly, missing/malformed IDs are
null. The 64th consecutive schema-invalid control receives its reply then
closes through the fatal path; a schema-valid control resets the counter.
Controls are handled serially and settle before the next is admitted, so there
is no pending-control ID history to retain: a settled ID may be reused. Durable
message submission idempotency remains separate. Inspect-worker and invalid-ledger
read-only recovery channels retain strict decoding. With healthy output, the worker cancels
pending work, drains a final snapshot and `worker.closing` with reason
`protocol_error`, then exits 74. A failed output transport can prevent those
final frames. The first event is `hello`, with protocol,
schema hash, protocol capabilities, phase `starting`, and random worker instance identity.
`session.ready` follows initialization; startup failures carry a typed
`startup.failed` code (including `lease_busy`). Subsequent event sequences
increase within that instance. Unknown event kinds and additive envelope fields
are allowed; permission-sensitive control parameters remain strict.
An occupied lease produces `startup.failed` with `lease_busy`, then closes the
worker; Stage 0 does not provide a read-only view through an occupied lease.

Rust emits the Swift Codable declarations and fixtures via:

```sh
cargo xtask protocol-fixtures
swift test --package-path apps/macos/RustyKit --jobs 2
```

Generation code and Swift templates are compiled only into tests, not the CLI.
`src/protocol_schema_hash.txt` is the only generated runtime constant.

## Commands and recovery

Requests have `protocol`, `request_id`, `method` and `params`. IDs contain
1–128 ASCII letters, digits, `.`, `_`, `:`, or `-`. Supported session
controls are `message.submit`, `run.cancel`, `approval.decide`, `snapshot` and
`shutdown`. `inspect` belongs on the separate inspection worker. Slash commands
are rejected as message submissions until their typed controls are implemented.

A message submission records a private durable receipt before queueing work.
Repeating an ID with identical text returns its receipt; different text under
that ID is rejected. Receipts are capped at 10,000 per session, with an 8 MiB
decoded JSON cap enforced before admission, reserving space for state changes.
Receipt ledgers use the existing session compression above its threshold; recovery accepts legacy plain JSON and gzip, with separate bounded physical and decoded reads.
An oversized legacy ledger opens the transcript read-only and marks recovery
unknown; it never guesses which operations ran. Snapshots return at most 100 receipts and label truncation;
a client can query a known submission by repeating the same ID and text.

After a crash, accepted/running receipts become `uncertain`; clean shutdown marks
queued work that never started as `not_started`. Recovery checks all receipts,
including those outside the recent page, and reports bounded uncertain runs and
in-flight tools with command summaries. They are never
replayed automatically. This prevents duplicate submissions; it does **not**
promise exactly-once external side effects across process death. Inspect prior
effects before explicitly submitting new work. Transcript writes retain the
existing session flock, and another CLI/client cannot steal it.
Tool uncertainty is represented by `session.snapshot.uncertain_tools` and its
count/truncation fields, rather than a separate `tool.uncertain` event.

Snapshots include session identity, worker epoch, sequence watermark, bounded
history, current streamed text, last tool and the complete pending approval.
Use the watermark to discard already covered events.
Before sending a snapshot request, start buffering incoming events. Its reply
has a later sequence than its watermark, so events newer than the snapshot may
arrive before the reply; replay those buffered events after applying it.
Sequences are allocated before queue admission, so dropped events leave gaps.
The writer emits a resync marker after the queue drains even when no further
event is produced. Detect sequence gaps even before a marker arrives.
Live text is capped at 128 KiB per content/reasoning stream and explicitly labels truncation; completed
transcripts remain authoritative. Deltas coalesce to approximately 30 Hz.
A 256-event queue has a separate 16 MiB budget for retained serialized buffers,
including encoding reservations and the frame being written. The budget charges
buffer capacity, including newline space; it does not bound caller-built JSON
values or transient allocator overhead. Serialization is bounded to 4 MiB per
frame. Admission may drop early to reserve one maximum-sized encoding buffer.
Dropped-event markers include counts and at most 16 dropped reply IDs with an
explicit truncation flag. A missing reply never implies its operation succeeded.
The client must fetch a snapshot after sequence gaps; receipt queries recover
submission outcomes. Backpressure does not cancel work. Transport failures exit
nonzero; normal shutdown drains the writer with a bounded deadline and emits
`worker.closing` when the channel permits.

Approvals bind a random ID to an operation digest including tool arguments,
working directory, run and destructive classification. They expire after five
minutes; wrong digests, repeated answers and stale decisions are rejected.
Concurrent requests serialize approval cards instead of being auto-declined.
Each operation identifies its requester; explicit decline, expiration and
interruption have distinct outcomes. Approval payloads are sensitive and exact;
ordinary tool events redact arguments.
Destructive approval requires the engine's exact `yes` phrase. Worker mode
scrubs and ignores `RUSTY_ALLOW_DESTRUCTIVE`, even in yolo mode.

Cancel is bound to the active run ID and checks again before tool admission.
A finishing run refuses a late cancel; `run.completed` is the authoritative
outcome. `tool.proposed` precedes authorization and `tool.started` means admitted.
Already admitted external effects may finish; Stop cannot roll back a write or
remote operation. EOF/shutdown stops work; workers do not continue after app
quit. A background supervisor is a separately reviewed later stage.

## Read-only inspection

`tree.snapshot`, `file.read`, `diff.files`, `diff.hunks` and `diff.expand` use
explicit paging and generation hashes. File reads reject symlink components and
root escapes using directory-relative file descriptors. Inspection currently
supports UTF-8 filenames and repository-root HEAD-to-working-tree aggregate
diffs. Unsupported encodings/binary files, oversized files and stale generations
are explicit results/errors. Staged/unstaged separation and arbitrary revisions
are later work.

Git inspection disables external diff, text conversion, clean/process filters
and fsmonitor programs, clears environment path overrides, uses literal pathspecs
and argument arrays, and has deadlines and output caps. Intraline spans are UTF-8
byte offsets in `[{start,end}]` arrays using bounded linear prefix/suffix
comparison; several changes on one line may share a larger emphasis span.
Unavailable/truncated emphasis is null. Every inspection result is capped at
256 KiB of serialized JSON, including escaping and metadata. Huge rows truncate
explicitly on scalar boundaries and still advance. Diff continuation uses
`next_cursor:{hunk_index,row_offset}`; resume that exact cursor with the same
generation. `next_offset` does not imply a partial hunk has completed. `.git`
case variants and inode aliases are blocked; symlinks remain listed but are
never followed for content inspection.

## Qualification

Run `cargo xtask qa`, the Swift protocol tests, and the separate
`apps/macos/DiffPrototype` test/measurement commands. `cargo xtask cli-gate
CANDIDATE [BASELINE_BINARY] [BASELINE_SOURCE]` checks the dependency/resource boundary, reports size delta,
and compares macOS linked libraries when a binary baseline is supplied.
A source baseline additionally qualifies the locked production dependency tree;
without it the dependency comparison is explicitly unqualified. Resource checks
cover production `include_str!`, `include_bytes!`, and `build.rs`; exact test-only
code generation is excluded.
`cargo xtask fixture-provider REPLIES.json` serves a bounded sequence of local
SSE responses for native journeys; it never reaches a paid provider.

The native diff prototype is a technical spike with synthetic fixtures. Its
README and receipts distinguish model tests, offscreen drawing and actual GUI
checks from unmeasured scroll hitches, full accessibility and real-worker UI
integration. A native inspector, chat, session management and signed distribution
remain subsequent stages requiring review and executed evidence.

## Pinned protocol names

The implemented contract is authoritative: idempotency uses envelope
`request_id`; cancel takes `run_id` with run-only scope; approval decisions use
`allowed` and optional `typed`; content ends with `message.completed`; usage is
`run.usage`; snapshots are `session.snapshot` with `snapshot_watermark`.
Diff rows use `context`, `addition`, `deletion`, `old_line`, `new_line`, and
`no_newline`. Diff base is repository-root HEAD to working tree. Generated
event payload types and command types both contribute to the schema hash.

Worker startup uses a flag/local context-window table or the conservative
default, without a provider models probe. Memory-advisor connection is lazy on
the first run. `RUSTY_WORKER_TRACE=1` writes measured per-phase and total startup
times to diagnostics; it creates no provider requests.

### Reproduce the recorded worker journey

Use a fresh output directory and an already built candidate binary:

```sh
python3 scripts/native_worker_journey.py /absolute/path/to/rusty \
  --out /absolute/path/to/new-journey-evidence \
  --record-fixture apps/macos/RustyKit/Tests/RustyProtocolTests/Fixtures/recorded-worker.ndjson
```

The standard-library script starts a loopback-only scripted SSE provider, edits
`calc.py` through the worker, approves the exact operations, executes an assertion,
and inspects the resulting Git diff through a separate provider-free worker.
It records actual wire events; the optional fixture replaces temporary-directory
strings with `<TEMP>` while retaining unnormalized evidence separately. Receipts
include source and binary hashes. The journey has a 90-second deadline, three
model requests, bounded frames, requests and retained diagnostics, and owned
process/server cleanup on failure. It spends no provider credits. Run the Rust
and Swift recorded-wire contract tests after refreshing the fixture. This is
worker qualification, not a native-app or live-provider E2E result.

Native session workers preserve their launcher's terminal signal disposition.
They do not install the interactive CLI's first/second Ctrl-C handler or save
terminal state; clients cancel through `run.cancel` and close through `shutdown`.
The ordinary CLI and tool-RPC signal paths retain their existing behavior.
