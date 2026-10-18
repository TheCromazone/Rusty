# Native task notifications: required behavior and qualification

Status: required product feature, not implemented in the current diff prototype.
The current prototype displays synthetic diff rows; it has no session supervisor,
notification settings, operating-system delivery or task-navigation shell.

## User controls and wording

Each task offers “Notify me when done”. Notifications require an explicit opt-in;
sound is a separate opt-in. Completion, failure and approval-needed attention
must be distinguishable. Show in-app attention when the relevant session window
is key and the app is active; suppress redundant background banners for that view.
Coalesce simultaneous session completions instead of reporting every tool or
swarm member separately.

Use an authoritative runtime outcome. `Agent::goal_outcome` distinguishes verified,
completed-unverified, blocked and incomplete goals. A successful model turn or
`run.completed` event alone does not establish that the user's task is complete.
Without authoritative task completion, use “Response ready”. Cancellation,
interruption and uncertain effects must not become success notifications.
The native wire contract must expose the required outcome and verification fields
before the app consumes them; update Rust, generated Swift, fixtures and schema
hash together.

## Privacy, routing and bounded ownership

Default text is generic and private. Do not put prompts, command output, file
contents, credentials, exception text or absolute paths on the lock screen.
Session names are user input; omit them by default and sanitize any opt-in label.
Notification actions only open the exact session/run or pending approval card.
They never approve, deny, retry or execute an operation. Stale destinations must
explain their state rather than opening another task.

Deduplicate using stable semantic outcome IDs and authoritative snapshot state.
Keep storage bounded across restarts; no global, ever-growing event-ID set.
On a new worker epoch, establish a snapshot baseline before enabling live alerts.
Do not replay historical completion events or turn a sequence gap into a guessed
success. Coalesce pending attention and bound retained delivered notifications.
Release event subscriptions, closures and pending delivery owners when detached.
Permission denial, disabled notifications and Focus mode have a truthful in-app
fallback; successful enqueue is not proof that a banner or sound was delivered.

## Implementation order

1. Finish the open worker signal, snapshot-size, cancellation and recovery gates.
2. Pin authoritative outcome semantics in the worker contract and add a pure Swift
   policy component to RustyKit, using recorded real-worker fixtures. This is
   component qualification, not delivery E2E.
3. Optionally create an isolated bundled notification test host. Record permission,
   denial/settings behavior and category/action routing separately from the product.
4. After the native product's next-stage approval, wire the real session owner and
   windows to macOS notifications. Keep GUI and notification dependencies out of
   the Rust CLI. A CLI terminal ping belongs in a separate change.

The current Stage 0 prototype must not grow a synthetic session supervisor just
to claim notification support. Guaranteed delivery after quitting the app is a
separate background-lifecycle feature and is not implied by remote execution.

## Acceptance evidence

- Policy tests cover verified/unverified completion, ordinary turns, failure,
  blocked/cancelled/interrupted/uncertain outcomes, approval-needed attention,
  foreground suppression, sound/opt-out controls, stale routing, replay, gaps,
  reconnect, bounded retention and ownership release.
- A fixture journey uses a real Rust worker outcome and the actual Swift policy,
  with exact session/run correlation. A mock notification sink proves only the
  component boundary.
- Full E2E uses the bundled native app: grant or deny permission, background a real
  task, observe the actual banner or Notification Center entry, click it, and
  verify the correct task opens. Exercise approval-card routing without granting
  permission, reconnect without duplicate pings, and disabled settings fallback.
  Observe optional sound separately; a visible banner does not prove audio.
- Repeat completion/reconnect/close cycles and inspect callback/task/process/FD
  ownership and memory retention. Preserve exact source/app identity, OS settings,
  receipts and failures. External accessibility, compositor, physical footprint,
  signing/distribution and quit-app delivery remain separate evidence gates.

Screenshots of the current diff prototype demonstrate only that prototype;
they must not be presented as screenshots of working notification settings.
