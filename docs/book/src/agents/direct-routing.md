# Direct channel routing

Direct routing optionally asks the existing `typesafe__typesafe_system_one`
tool to choose an agent before the channel owner's reasoning model runs. The
selected agent processes the ordinary channel turn, responds in the same chat
and topic, and retains ownership for follow-ups. This does not run a delegate
task and does not change any agent's tools, autonomy, or approval policy.

The channel's existing explicit binding remains authoritative: only its owner
can enable direct routing for that exact channel key. Configure intent
descriptions under that owner, for example:

```toml
[agents.main.direct_routing]
enabled = true
channels = ["telegram.example"]
timeout_ms = 1500
min_probability = 0.8
min_confidence = 0.6

[agents.main.direct_routing.candidates]
coding = "Substantial software implementation, debugging, and repository work."
youtube_creator = "Original video scripts and local video draft rendering."
```

Candidate descriptions describe intent; they grant no access. The runtime
intersects these names with the owner's current permitted delegation roster,
accepts only targets already configured for independent execution, and retains
the owner as fallback. Bounded delegates continue through the owner's existing
delegation workflow. A missing, disabled, unapproved, or changed target is never
made eligible by a model answer. Changes to materialized configuration require
the normal channel-context reload before an existing specialist context can be
selected again. The owner's policy must permit the `delegate` operation without
an approval prompt, as well as permit the TypeSafe tool itself.

This initial implementation accepts sender-scoped conversations. Telegram
group chats are excluded, including their topics; private Telegram topics are
isolated using the existing canonical conversation key. Other channels require
their own explicit exact-key opt-in. Runtime commands, passive observations,
internal SOP events, attachments, and queued recovery do not trigger a new
classification. An existing selected agent still handles its normal commands,
including `/new`.

## Persistence and fallback

Only a new conversation with no durable or cached history is classified.
Existing conversations remain with the bound owner until `/new` or a new topic.
The native SQLite session metadata stores the selected agent, including an
owner fallback, and restores it after restart. JSONL and nonpersistent sessions
do not enable direct routing. `/new` clears the current session using the normal
command path; its next ordinary message can be classified again.

Responses must name an offered choice, carry the advisory-only flags, and have
valid probabilities, a highest-probability winner, and confidence meeting the
configured thresholds. These thresholds are operational choices, not proof of
correctness. Missing tools, approval requirements, timeouts, invalid responses,
low confidence, and persistence failures fall back to the bound owner. The
classifier is not retried. Every proposed or persisted specialist is checked
against current eligibility before running.

Routing runs within the serialized conversation worker, before its agent turn.
One slow classifier cannot block the listener or `/stop`. Durable turn ownership
can change while received or queued and becomes immutable once execution starts.

## Data and permissions

The routing call sends only the current plain-text request and configured
rubrics. It sends no conversation history, memory, attachments, or workspace
files. Both raw input and encoded arguments are limited to 8 KiB. Requests
containing recognized credential patterns or credential terms are kept local
and use the normal owner. This conservative guard may also skip harmless
requests about authentication. It is not a general personal-data detector;
ordinary request text is sent to TypeSafe when this feature is enabled.

An ingress hook that might redact or cancel the request prevents fresh routing;
its ordinary turn behavior remains in force. Tool execution uses the owner's
admitted MCP tool, current grant and policy, approval manager, cancellation,
and emergency-stop controls. The tool must already be eager or activated;
routing never discovers or installs a missing tool. Logs contain the selected
agent and whether a judgment was accepted, not request or answer content.

A route never authorizes sending, publishing, purchasing, deleting, or changing
permissions. The selected agent's existing restrictions continue to apply.
Model-provider fallback is configured separately on each agent; routing does not
choose a provider or add a fallback model.
Disable `direct_routing.enabled` and reload channels to restore ordinary owner
routing while keeping the TypeSafe tool and all specialist configuration.
