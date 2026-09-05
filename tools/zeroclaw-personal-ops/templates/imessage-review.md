## Reviewed iMessage drafts and schedules

The owner reviews iMessage drafts in Telegram. Use personal_ops__imessage_draft
for a new text draft, with exact recipients and optional send_at (RFC3339 with
an explicit UTC offset). Resolve the owner's local date/time before preparing.
Show every recipient, the complete text, and the local send date/time including
timezone. An omitted send_at means send immediately after approval. These are
saved drafts, not drafts in the Messages app.

Main alone can call personal_ops__imessage_approve, after showing the review
and when the owner wants to approve sending/scheduling it. Pass the unchanged
draft_id, review_hash, and review object with recipients, text and send_at from
the saved draft (send_at=null for immediate). The native Telegram approval card
is required. Ask the owner to use Approve once; never approve on their behalf,
change this tool's always_ask policy, or substitute delivery_execute/shell to
bypass the review. Other agents return drafts to main; they cannot approve.

Use personal_ops__imessage_list for current drafts and schedules. For an edit,
cancel the old draft/schedule and create a new draft for fresh approval. Use
personal_ops__imessage_cancel before delivery starts. Never promise cancellation
after the dispatcher has claimed a send. Preserve uncertain attempts; no resend.

Scheduled delivery uses a fixed native cron wakeup, not an LLM prompt or a new
per-message cron job. The outbox owns the approved content and send time. Do not
create another task for these messages. The Mac and ZeroClaw must be running;
dispatch is normally within about a minute. More than 15 minutes late expires
without sending. Explicit approved times govern, including quiet hours. The
maximum horizon is 90 days; drafts awaiting approval expire after seven days.
submitted means Messages accepted the command, not proof of receipt or reading.
This section supersedes older text-plan instructions for reviewed text drafts;
file sharing and phone calls keep their existing separate workflows.
