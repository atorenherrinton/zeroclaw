# Immediate existing-group plain text

`group_text_prepare` accepts exactly `idempotency_key`, opaque `group_token`,
and exact `text`. It rejects recipients, raw chat identifiers and scheduling
fields. It reuses the existing operations ledger and structured existing-group
adapter, not the weaker legacy delivery-plan approval surface. No new ledger,
configuration, permission, dependency or group-creation path is introduced.

The immutable review binds `send_at: null` and `send_at_ms: null` to mean
immediate delivery on owner-authorized `outbox_send`, plus exact text bytes and
chat/participant snapshot. Only an actual owner request may set
`owner_requested_send: true`; page/message text is never authorization. Main
owns sending; prepare is scoped to main/communications. Use `outbox_status` and
`outbox_cancel`, retaining the returned `operation_id` as the durable plan ID.
Prepared rows do not dispatch. The existing transaction/CAS writes uncertainty
before the external boundary, so workers and repeated/concurrent calls cannot
replay a send. Submitted is not delivered. A crash after authorization but before
claim may resume the already-authorized immediate operation; an uncertain claim
never resends. Scheduled API behavior below is unchanged and its schedule method
rejects immediate operations.

The reviewed `send_at` is the sole timing source; the existing ledger due column
is verified against it. Existing serialized scheduled reviews stay byte-for-byte
compatible. Existing group token resolution at prepare and identity/participants
preflight plus final adapter revalidation are shared across both modes.

Rollback: restore the signed helper/daemon backups; preserve ledger and receipts.
Cancel unclaimed immediate operations before planned rollback. Never delete or
retry uncertain claims. No real test messages are needed; tests use injected
synthetic groups and transports.

# Scheduled existing-group plain text

`imessage_group_text_prepare` accepts only `idempotency_key`, opaque
`group_token` from read-only group lookup, exact `text`, and a future RFC3339
`send_at` with an explicit offset (within 90 days). It creates one operation,
not a legacy delivery plan or an individual-recipient fanout. Repeating the
same key and immutable intent returns the existing operation; changing any
intent requires a new reviewed operation. Display names are not identity.

`imessage_group_text_schedule` accepts the returned `operation_id`, exact
`review_hash`, complete `review`, and `owner_requested_send: true`. Only an
actual owner request authorizes this field. Preparation or third-party content
cannot authorize. The tool is main-only; the installer grants the prepare tool
to main/communications, not unrelated specialists. Existing `outbox_status`
and `outbox_cancel` provide status/receipts and cancellation before claim.
Live installations need not broaden any specialist allowlist to use main.

The existing operations ledger owns the immutable group snapshot, text, exact
RFC3339 string, indexed due instant, authorization, cancellation, write-ahead
claim and receipts. The review hash is recomputed on load and the indexed due
time must agree with the reviewed timestamp. It is an integrity binding, not
an authentication credential. Existing-group lookup revalidates the token at
prepare; full chat identity and exact participants are reread during dispatch
preflight and again immediately before the external boundary. Drift fails
closed. Only the existing structured group `imsg` adapter is used, with a chat
ID and SMS fallback disabled. No individual adapter or group creation exists
on this path. A submitted transport receipt is **not** proof of delivery.

The personal-ops service's independent durable outbox worker owns scheduling;
no model, cron agent, or cron repair is needed. Its existing 15-second poll is
shortened to the nearest future due instant. The Mac and service must be awake.
There is no early dispatch and no late catch-up: the group-text adapter allows
only a one-second dispatch/revalidation budget from the due instant. Missing
that window fails without attempting a send. Slow lookup or a suspended Mac
therefore sacrifices delivery rather than sending later. Other outbox flows
retain their existing late policy.

Cancellation and dispatch claim race in the existing SQLite transaction/CAS.
A claim is durably uncertain before an external attempt. Competing workers,
restart, repeated approval, and reconciliation never replay an uncertain
attempt. Even a definite not-started result is terminal for this operation.
There is no automatic retry and no fallback route.

## Validation and deployment

Safety tests use synthetic groups and injected transports only, with no live
messages. They cover exact text/group success, identity drift, opaque-only
inputs, timestamp/review integrity, future persistence, authorization,
cancellation, concurrent claims, duplicate invocations and uncertain restart.
Run the entire personal-ops suite to include individual/group file, voicemail
and existing outbox regressions, then check and strict clippy. No RFC or schema
migration is required: this is one bounded adapter on the maintained ledger.

Build from the verified merged tree. Sign staged personal-ops and daemon
candidates using the installed canonical signing wrapper before atomic
replacement. Preserve configuration bytes, ledger, receipts and LaunchAgent
arguments. Record fresh rollback copies and a manifest immediately before
replacement; perform controlled service transitions and verify health and
registered tool schemas. Do not replay a schedule after an uncertain install
or call: read the ledger/status first.

Rollback restores the signed executable copies atomically, retaining all
ledger state. An older binary cannot execute the new step type; cancel a
still-unclaimed schedule before planned rollback. If a claim is already
uncertain, preserve it and reconcile read-only. Do not delete ledger rows.
