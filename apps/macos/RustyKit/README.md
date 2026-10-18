# RustyKit protocol (stage 0)

This Swift package contains native-client transport types only. It does not call
providers or decide permissions, and adds no GUI dependency to the CLI.

Generate checked-in Swift types and fixtures from Rust's field/method declarations:

```
cargo xtask protocol-fixtures
swift test --package-path apps/macos/RustyKit --jobs 2
```

The generator runs only in an explicitly invoked ignored Rust test; no Swift
code or templates are compiled into the shipping CLI. Runtime handshake carries
a checked-in schema hash covering command and event payload types. Rust unit tests reject generated-file drift. JSONValue's checked-in support template
is embedded by the Rust generator. Rust fixtures are decoded and re-encoded by
Swift tests, including Unicode, typed approval and unknown-cost state.
`Fixtures/recorded-worker.ndjson` is a verbatim capture from an executed local
scripted-provider journey, separate from generated fixtures. Both Rust and Swift
validate its actual lifecycle, approval and tool frames. It contains synthetic
test paths and values; it is not evidence of live-provider or native-app E2E.

The private pipe is NDJSON, at most 4 MiB **including newline** per frame. EOF
with an unfinished frame is an error; EOF at a frame boundary is a disconnect.
The supervisor must stop work when transport is lost. No stdout parsing is used.
`protocol` is version 1. A hello event carries `instance_id`, `seq`, schema hash
and capabilities; compare the hello hash with generated `protocolSchemaHash`
before submitting controls. Sequence values are meaningful only within one host instance.
A snapshot's watermark belongs to that instance.
Start buffering events before requesting a snapshot. After applying the reply,
discard buffered events at or below its watermark and replay newer ones; they
can arrive before the reply. Sequence gaps require another snapshot even before
the next emission can deliver a resync marker. Unknown protocol versions or
methods fail closed. Unknown envelope fields and event kinds are tolerated;
security-sensitive command parameters stay strict in the Rust authority. New
optional parameters must decode when missing or null. Request ids have 1..128 ASCII letters, digits, `.`, `_`, `:`, or `-`.

`message.submit` accepts text; `run.cancel` requires the exact run id;
`approval.decide` carries approval id, operation digest, Boolean decision and
optional typed confirmation. Stage 0 exposes no broad always-allow decision.
`snapshot` and `shutdown` accept empty objects. `inspect` wraps an operation
(`tree.snapshot`, `file.read`, `diff.hunks`, `diff.expand`) plus its arguments.
Inspections use bounded pagination and may carry `expected_generation` to reject
stale views. Inspection results remain engine-owned JSON values; pinned row names and
per-hunk continuations are documented in `docs/NATIVE_APP.md`. Lifecycle and
approval payloads have generated Swift structs through `Event.decodedPayload()`.

A submission receipt records acceptance, not exactly-once tool side effects.
After a crash, uncertain completion must be shown as uncertain, never replayed.
EOF/shutdown is the stage-0 lifecycle; persistent background sessions are later.

No app UI, signing or distribution is implemented by this package.
