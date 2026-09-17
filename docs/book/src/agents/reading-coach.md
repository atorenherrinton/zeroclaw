# Limited reading coach

The `reading_coach` tool and the template in
`examples/agents/reading-coach/` provide a small, age-appropriate reading helper.
This is not a general autonomous agent. The template is disabled, has no channel
or scheduled job, and grants exactly one non-sending tool.

## Behavior and boundaries

The assistant asks how many pages remain, the due date, and the time zone. The
Rust tool returns questions instead of inventing missing facts. After each
progress report it recomputes:

```text
reading_days = max(local_due_date - local_today + 1, 0)
required_average = remaining_pages / reading_days
suggested_daily_pages = ceiling(required_average)
```

The last reading day is **inclusive, through local end of day**. For a before-school
deadline, confirm the previous day as the last reading day. Today counts as a
reading day; the daily target is a suggestion, not a requirement to catch up late
at night. Stop at zero pages. An overdue plan has no finite required daily rate
and recommends replanning with a parent or teacher, not nagging or rushing.
Calendar days use the confirmed IANA zone, including daylight-saving transitions,
not the server's date or a fixed number of seconds. Input is bounded to 10,000
remaining pages and a due date no more than 366 days ahead. Page counts are whole,
non-negative integers. Unknown fields, malformed dates, invalid zones, and future
delivery receipts fail closed.

A reminder block is optional and requires explicit `enabled`, `paused`, and
`local_hour` fields. Eligibility is limited to one agreed daytime hour (08:00
through 19:59 locally), an active plan, no successful delivery on the current
local date, and at least 20 elapsed hours since the last successful delivery.
No catch-up burst occurs outside that hour. Paused, disabled, completed, overdue,
or incomplete plans never recommend a check-in. Quiet-hour behavior is enforced
in Rust, not only in a prompt.

**Eligibility is only a recommendation.** The tool has no network, file,
scheduler, memory-write, or sending side effects and does not reserve a delivery
slot. Concurrent evaluations can return the same recommendation. The existing
scheduler/delivery receipt store, not this stateless tool, remains responsible
for single delivery and uncertain-send reconciliation. Do not retry an uncertain
send or record a generated draft as successfully delivered.

## Owner-led onboarding

Before enabling the template, obtain:

1. The owner's approval and the young reader's agreement to check-ins, with a
   clear way to pause or stop. A trusted adult handles configuration.
2. The reading task (title optional), confirmed pages remaining, and the deadline
   convention. Do not infer the page count from a book's title or edition.
3. A confirmed IANA time zone. Do not collect an address, school, birthday, or
   location tracking to establish it.
4. The exact owner-approved private channel/account and recipient binding,
   verified using existing channel allowlisting. Never use a wildcard sender,
   a group, a default contact, or an invented destination.
5. The agreed daytime check-in hour and frequency (at most once daily), and a
   trusted adult's process for pausing delivery and updating progress.
6. An existing owner-approved model-provider alias and its data-handling policy.
   Do not copy another agent's private conversation or memory into this agent.

Merge `config.fragment.toml` into the existing config; do not replace the config.
Set the model-provider alias, and copy the template `AGENTS.md` into the resolved
`reading_coach` agent workspace. Keep `enabled = false`, `channels = []`, and
`cron_jobs = []` until onboarding is complete. Validate the merged configuration
before reloading. Do not grant MCP bundles, skills, knowledge bundles, delegation,
or additional tools. The non-empty `allowed_tools = ["reading_coach"]` is
important: an empty allowlist in a named risk profile means unrestricted, not
deny-all. Existing channel authorization must bind inbound messages to this
isolated agent, not the owner's general-purpose assistant.

The template uses the existing bounded tool loop (`agentic = true`, maximum three
iterations). That setting permits calculation, not general autonomy. Profile
allowlisting is tested with the real security policy, independently of the prompt.
Language-model wording is not a hard content filter; adult oversight remains
necessary. The prompt discourages shame, secrecy, spoilers, sensitive-data
collection, and inappropriate content, and redirects unrelated requests.

## Delivery integration

This release supplies the reusable agent and planning/check-in capability, **not
an automatically enrolled reminder service**. The reader cannot create cron jobs
or send messages. Once onboarding is complete, the owner may configure the
existing scheduler outside the child's agent, with explicit recipient consent.
Do not configure unconditional automatic delivery of every tool result: false
eligibility and pause must mean no outbound message. Until a delivery controller
can persist pause/stop state and check eligibility atomically with existing
receipts, use owner-reviewed check-ins only. The assistant must not claim it
paused an external schedule merely because it acknowledged a request.

Keep confirmed task facts in the isolated conversation/owner-managed task record
and actual sends in existing delivery receipts. The tool stores no parallel
state. Recalculate from the latest confirmed remaining count, label stale
progress, and ask for an update rather than fabricating progress. For example,
101 pages across four reading days means an average of 25.25 and a suggested
26 pages per day; 61 pages remaining the next day means about 20.33, rounded to
21. A short check-in can ask, "How many pages are left now? We can adjust the plan."

## Validation and rollback

Run `cargo test -p zeroclaw-tools reading_coach --lib`,
`cargo clippy -p zeroclaw-tools --all-targets -- -D warnings`, and the repository
format/check gates. Tests exercise the public tool boundary, real profile
allowlisting, calendar arithmetic, and reminder suppression without contacting
anyone. Run the maintained repository checks before merging and report actual
results, including whether hosted CI ran. No client-side hook is required or
used as a substitute.

Disable the agent and remove any owner-created schedules to stop future use.
The capability itself creates no schedule or delivery state to undo. For a
binary rollback, restore the backed-up executable and reload/restart through the
normal maintenance workflow. Preserve receipts and unrelated scheduled work.
