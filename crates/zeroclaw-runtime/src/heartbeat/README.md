# Deadman notification lifecycle

The daemon owns the heartbeat worker and its watchdog in one selected future.
Worker exit, cancellation, and config reload drop the watchdog and any pending
delivery future. An already accepted channel message cannot be recalled by
cancellation.

`Config.heartbeat` is the notification policy for the current daemon generation.
`deadman_timeout_minutes = 0` mutes this notification after config reload; normal
heartbeat tasks remain enabled. Config writes alone do not update an existing
daemon generation. There is no heartbeat quiet-hours or acknowledgement policy
in the current schema; those remain follow-up work.

The `heartbeat_deadman` row in the existing
`<data_dir>/heartbeat/history.db` owns the monitoring baseline, completed-tick
generation, and most recent notification attempt. Live metrics and per-task
history are observational and cannot settle/re-arm an incident. First startup
records a baseline so failure to complete any initial tick is monitored too.
Reload/restart preserves that baseline rather than postponing the deadline.
Every worker generation checks the durable baseline immediately, before polling
the worker. Repeated startup failures shorter than the polling interval therefore
cannot starve an overdue incident. Subsequent checks remain one minute apart.
If the worker fails while that first delivery is pending, the attempt is
cancelled and remains uncertain; the initial poll does not guarantee delivery.

After the configured interval is exceeded, a SQLite update atomically claims
the current tick generation and commits `unknown` with full synchronization
before calling the channel. Repeated checks, concurrent connections, timeout,
cancellation, and process death cannot claim that same generation again. A
successful delivery callback records `delivered`; this reflects the delivery
adapter's acknowledgement, not a separate readback from the destination.
The watchdog requires a registered delivery adapter; an absent handler retains
`unknown` and can never be recorded as `delivered`. Ordinary cron announcements
keep their existing missing-handler behavior.
Failure or timeout retains `unknown`, without automatic retry. A crash between
claim and send may suppress an alert that was never sent; avoiding duplicate
uncertain delivery takes precedence over guaranteed notification.

Only an actual completed worker tick re-arms monitoring, including completed
failed ticks, empty task lists, and a two-phase decision to skip. The first tick
after an incident records one internal recovery log; it sends no external
recovery notification. Muting, unmuting, or reloading does not itself re-arm an
already claimed incident. Persistence errors propagate to the heartbeat
supervisor; no alert is sent without a committed claim.

Tests exercise real SQLite files and competing connections, a child process
that exits after claiming but before saving delivery outcome, and virtual-clock
worker cancellation, delivery timeout, and mute behavior. No real channel send
is needed for these checks.

The table is additive and existing task history is preserved. Older binaries
ignore the table and retain the previous repeated-alert behavior. Set the narrow
timeout mute before downgrading; keep the database and newer delivery claims.
Deleting watchdog state to force a retry can repeat an uncertain delivery.
