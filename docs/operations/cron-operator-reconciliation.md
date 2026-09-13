# Exact cron operator reconciliation

This is a narrow repair of the existing scheduler quarantine contract, not a new
scheduler or a retry mechanism. The canonical execution and delivery facts remain
in `cron_runs` and `cron_occurrences`. `cron_reconciliations` creates a **new fact**:
an operator's source-attributed no-external-effect assertion. It does not replace
or relabel the original occurrence's uncertainty or idempotency receipt.

## Operator boundary

Privileged local operators can use:

```text
zeroclaw cron reconciliation-status EXACT_JOB --run-id EXACT_NUMERIC_RUN --occurrence-id EXACT_OCCURRENCE
zeroclaw cron reconcile EXACT_JOB --run-id EXACT_NUMERIC_RUN --occurrence-id EXACT_OCCURRENCE --expected-state STATE_HASH --disposition no_external_effect --evidence 'Verified operator evidence'
```

The API provides GET/POST
`/admin/cron/{id}/runs/{run_id}/reconciliation`. GET requires the exact
`occurrence_id` query parameter. POST requires `occurrence_id`, `expected_state`,
`disposition`, and `evidence` (1–4096 bytes). Unknown fields/dispositions reject.
Both methods require a loopback TCP peer, `X-ZeroClaw-Operator: cron-reconcile`,
**and a valid paired bearer token with pairing enabled**. Browser Origin and
Sec-Fetch-Site headers reject. Forwarded IPs do not grant privileges; a loopback
reverse proxy does not bypass paired authentication. The native CLI uses the
existing privileged local OS-user boundary and requires no token extraction.
The transport fixes source attribution; a request cannot supply a source.

Neither operation is a model tool. A cron prompt, result, or allowed-tool list
cannot expose or authorize it. An operator must independently establish zero
external effect; this API cannot prove the truth of a human assertion. Ordinary
`cron_update` remains unable to release an uncertain job. Keep privileged shell
access restricted to trusted operator contexts.

## Transaction and replay behavior

The state hash binds the raw current definition, exact run, exact occurrence,
lock state, latest-run check and ambiguity check. A FULL-synchronous IMMEDIATE
transaction checks that hash again, inserts the receipt, then releases only the
job's quarantine status. Both changes commit together or neither does.

The supported case is deliberately conservative: disabled, unlocked, recurring,
quarantined job; latest terminal error run with matching last-run time/output;
exact possibly-applied occurrence with matching output and no requested delivery;
no other ambiguous pending occurrence. Other cases require investigation, not a
broader disposition or replay override. An expired one-shot cannot be reconciled
through this path. Receipt identity is unique by job/run and job/occurrence.

Identical requests from the same source return the same persisted receipt without
releasing any new quarantine. Conflicting repeats reject. The original occurrence
is never modified or deleted. Its old request key still returns its original
duplicate/uncertain receipt, **not execution**. Manual admission discounts only the
exact reconciled occurrence at its unchanged ledger version; any subsequent
change to that version restores the uncertainty gate.

Reconciliation does not enable, advance the schedule, run, send, or change tools.
Enabling with `missed_run_policy=skip` is a separate operation that sets a future
next-run. A fresh manual verification must use a new stable request ID. Never
retry a submitted/uncertain verification. Status reads use a read-only connection
and never create or migrate storage; receipts always say `retry_allowed=false`.

## Validation and rollback

Tests cover exact release, immutable history/definition, disabled state, separate
future enable, job/run/occurrence mismatch, stale state, multiple uncertainties,
unsupported dispositions, conflicting/identical duplicates, read-only durable
status, failed receipt/gate atomicity, old-key no-replay, renewed uncertainty,
CLI schema, paired local-only API authorization and absent model-tool registration.

Binary rollback is safe and conservative: the old runtime ignores the new table
and may quarantine manual execution again because it still sees the unchanged
uncertain occurrence. Never drop receipts or edit scheduler storage to force it
through. If rollback follows enabling, pause the exact job through the supported
API/CLI before reverting. Preserve configuration bytes and all scheduled state.
