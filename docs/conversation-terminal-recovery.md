# Conversation terminal recovery

A running process is not evidence that a conversation finished. The durable channel turn records generation failures in `tasks.error`, saves a terminal response before submission, and records acknowledgement separately. A delivered failure notice has `status=delivered` and a retained generation error. An unacknowledged notice remains `uncertain`; a confirmed subset is `partially_delivered`.

## Ownership and boundaries

- The existing task journal owns phase, output, error, and delivery facts. No additional response database is introduced.
- Model calls report an elapsed wait every 30 seconds. The timer does not call a provider or tool again. Telegram receives a typed waiting state rather than an obsolete tool activity. Provider errors retain a bounded, scrubbed operator diagnostic separately from their generic model-visible aggregate.
- Tool result admission measures source evidence and nested history envelopes before downstream copies. Explicit read adapters and the shell adapter may return marked incomplete text previews; this is not evidence of non-execution and never authorizes replay. The runtime does not discard structured payloads, failure causes, or original write results to make them fit. Oversized exact results transfer intact into `ResultBudgetExceeded`, retaining receipts, while terminal recovery still reports the interrupted turn truthfully.
- Impossible envelope budgets, oversized identifiers, and oversized receipts still fail closed. The channel failure path delivers a truthful notice rather than treating an admission rejection as a successful task.
- Every sixth existing model iteration requests a short findings/uncertainty update. It does not add a model call. Background delegation tells the parent to leave the assigned scope to the delegate and use a distinct task or wait.
- Error, deadline, and cancellation paths use the same saved-response/submission/acknowledgement ordering. They replace progress with a terminal notice and retain available partial assistant text. Auxiliary draft consumers are stopped before finalization, preventing late progress from overwriting a final notice.
- Model and tool work retain their inherited deadline. Delivery has a bounded 15-second cleanup phase; the outer worker guard allows 25 seconds for draining and delivery. This is not a new budget for executing actions.

## Completed delegates

Delegate admission captures the parent turn ID and trusted conversation route, without accepting a model-supplied destination. Saved-result lookup matches channel instance, recipient, sender, and thread. Reading results never marks them delivered.

When a parent stops, a bounded watcher checks existing delegate records for up to one hour. It resolves the live channel instance, claims the existing delegate row's `idem_key` before sending its saved excerpt, and marks `delivered` only after a positive channel acknowledgement. It does not run a model, tool, deployment, or message-producing task again. A lost acknowledgement leaves the claim quarantined rather than triggering a resend. After a process restart, unsurfaced results remain available to the next request in that conversation; the watcher itself is not restart-resumable.

## Validation and limitations

Regression tests exercise failed generation through conversation dispatch and SQLite checkpoints, positive and lost acknowledgements, cancellation context, inherited deadlines, real Telegram HTTP request encoding against a local mock, tool execution through final conversation delivery, encoded batches, provider wait progress, and delegate result persistence and notice claims across database reopening.

Output excerpts are not full evidence, and the model may not produce useful findings until its next response. Remote provider latency is not eliminated. If a platform cannot acknowledge a notice, the runtime cannot guarantee that remote progress was replaced. A process crash cannot synchronously edit a remote draft; active work remains quarantined and must not be blindly replayed. If a draft updater itself hangs past its drain budget, only partial text already retained by the runtime is available to the terminal notice.
