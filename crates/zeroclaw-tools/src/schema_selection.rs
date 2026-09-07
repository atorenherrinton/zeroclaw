//! Deterministic schema presentation from owner intent. This narrows the already
//! authorized registry; it never grants execution or infers authorization from
//! descriptions, tool results, or server-provided annotations.

fn words(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}
fn contains(words: &[String], candidates: &[&str]) -> bool {
    words.iter().any(|word| candidates.contains(&word.as_str()))
}

/// Baseline discovery and bounded reads remain available for uncertain intent.
/// Unknown tool names require an explicit name in the owner's request. Mutation
/// schemas also require action intent in the matching domain.
pub fn relevant(name: &str, intent: Option<&str>) -> bool {
    // None is used by explicitly unfiltered internal catalog assembly. Normal
    // model turns pass Some, including Some("") for missing owner text.
    let Some(intent) = intent else { return true };
    if matches!(
        name,
        "tool_search"
            | "memory_recall"
            | "sessions_history"
            | "sessions_list"
            | "file_read"
            | "glob_search"
            | "content_search"
            | "web_search_tool"
            | "web_fetch"
    ) {
        return true;
    }
    let request = words(intent);
    let tool = words(name);
    let explicit = intent
        .split_whitespace()
        .any(|word| word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_') == name);
    let text = intent.trim().to_lowercase();
    let explanation = [
        "explain ",
        "please explain ",
        "describe ",
        "what ",
        "why ",
        "how ",
        "can you explain ",
        "tell me how ",
        "show me how ",
    ]
    .iter()
    .any(|prefix| text.starts_with(prefix));
    let negated = ["don't ", "don’t ", "do not ", "never ", "without "]
        .iter()
        .any(|marker| text.contains(marker));
    let action = !explanation
        && !negated
        && contains(
            &request,
            &[
                "create",
                "add",
                "edit",
                "update",
                "delete",
                "remove",
                "send",
                "reply",
                "book",
                "schedule",
                "cancel",
                "move",
                "rename",
                "complete",
                "mark",
                "write",
                "fix",
                "implement",
                "run",
                "execute",
                "use",
                "build",
                "test",
                "commit",
                "open",
                "navigate",
                "click",
                "type",
                "install",
                "merge",
                "publish",
                "call",
                "reschedule",
            ],
        );
    let read = contains(
        &tool,
        &[
            "read",
            "get",
            "list",
            "search",
            "find",
            "lookup",
            "resolve",
            "history",
            "inspect",
            "status",
            "events",
            "availability",
        ],
    ) && !contains(
        &tool,
        &[
            "create", "add", "edit", "update", "delete", "remove", "send", "write", "set",
            "cancel", "move", "complete", "mark", "call", "patch",
        ],
    );
    if explicit && (read || action) {
        return true;
    }
    let coding_tool = matches!(
        name,
        "shell" | "file_write" | "file_edit" | "git_operations" | "delegate" | "codex_cli"
    );
    if coding_tool
        && contains(
            &request,
            &[
                "code",
                "coding",
                "repository",
                "repo",
                "file",
                "files",
                "bug",
                "tests",
                "test",
                "build",
                "commit",
                "implement",
                "implementation",
            ],
        )
    {
        return action;
    }
    let domains: &[(&[&str], &[&str])] = &[
        (
            &["calendar", "event", "events"],
            &[
                "calendar",
                "meeting",
                "meetings",
                "appointment",
                "appointments",
                "availability",
                "event",
                "events",
            ],
        ),
        (
            &["reminder", "reminders"],
            &["reminder", "reminders", "todo", "todos"],
        ),
        (
            &["imessage", "message", "messages", "telegram", "messaging"],
            &[
                "message",
                "messages",
                "imessage",
                "telegram",
                "conversation",
                "chat",
            ],
        ),
        (
            &["github", "git"],
            &["github", "repository", "repo", "commit", "code", "coding"],
        ),
        (
            &["browser", "safari"],
            &["browser", "safari", "website", "webpage"],
        ),
        (
            &["genealogy", "ancestry"],
            &["genealogy", "ancestry", "ancestors"],
        ),
        (&["phone", "voicemail"], &["phone", "call", "voicemail"]),
        (
            &["shipping", "tracking"],
            &["shipping", "package", "tracking"],
        ),
    ];
    domains
        .iter()
        .any(|(names, intents)| contains(&tool, names) && contains(&request, intents))
        && (read || action)
}

/// Follow-ups reuse the previous owner request solely for schema presentation.
/// This is not authorization: the canonical approval/tool scope still applies.
/// Callers provide owner-message text only, newest first, never tool results.
pub fn owner_intent<'a>(mut messages: impl Iterator<Item = &'a str>) -> String {
    let current = messages.next().unwrap_or_default();
    let current = &current[..current.floor_char_boundary(8192)];
    let request = words(current);
    let followup = request.len() <= 12
        && contains(&request, &["continue", "finish", "proceed", "yes", "again"]);
    if followup {
        for previous in messages.take(8) {
            let previous = &previous[..previous.floor_char_boundary(8192)];
            let previous_words = words(previous);
            if previous_words.len() <= 12
                && contains(
                    &previous_words,
                    &["continue", "finish", "proceed", "yes", "again"],
                )
            {
                continue;
            }
            // Bound schema-selection work independently of prompt size. This
            // derived string is neither a durable authorization nor a cache.
            let prefix = &previous[..previous.floor_char_boundary(8192)];
            return format!("{prefix}\n{current}");
        }
    }
    current[..current.floor_char_boundary(8192)].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_explanation_keeps_safe_discovery_without_mutation_schemas() {
        for name in [
            "shell",
            "file_write",
            "ops__calendar_create",
            "ops__phone_call",
            "ops__genealogy_lookup",
            "unknown_write",
        ] {
            assert!(!relevant(name, Some("Explain a hash table")), "{name}");
        }
        assert!(relevant("tool_search", Some("Explain a hash table")));
        assert!(!relevant(
            "ops__calendar_create",
            Some("Explain how to create a calendar meeting"),
        ));
        assert!(relevant("file_read", Some("Explain a hash table")));
        assert!(!relevant(
            "ops__calendar_create",
            Some("Explain ops__calendar_create")
        ));
        assert!(relevant("unknown_write", Some("Use unknown_write")));
        assert!(!relevant(
            "ops__calendar_delete",
            Some("Do not delete calendar events")
        ));
        assert!(!relevant(
            "ops__telegram_send",
            Some("Find messages without sending a Telegram message")
        ));
    }
    #[test]
    fn domain_reads_never_expose_adjacent_writes() {
        for (intent, read, write) in [
            (
                "Find tomorrow's calendar meetings",
                "ops__calendar_events",
                "ops__calendar_create",
            ),
            (
                "List my reminders",
                "ops__reminders_list",
                "ops__reminders_delete",
            ),
            (
                "Find that Telegram message",
                "ops__telegram_history",
                "ops__telegram_send",
            ),
            (
                "Research genealogy ancestors",
                "ops__genealogy_lookup",
                "ops__genealogy_update",
            ),
            (
                "Inspect my Safari browser",
                "ops__safari_inspect",
                "ops__safari_click",
            ),
            (
                "Review code in this repository",
                "ops__github_get",
                "ops__github_create",
            ),
        ] {
            assert!(relevant(read, Some(intent)), "{read}");
            assert!(!relevant(write, Some(intent)), "{write}");
        }
    }
    #[test]
    fn explicit_actions_select_only_the_matching_domain() {
        for (intent, tool) in [
            ("Fix the code and run tests", "shell"),
            ("Create a calendar meeting", "ops__calendar_create"),
            ("Complete that reminder", "ops__reminders_complete"),
            ("Send a Telegram message", "ops__telegram_send"),
            ("Open this website in Safari", "ops__safari_open"),
        ] {
            assert!(relevant(tool, Some(intent)), "{tool}");
            assert!(!relevant("ops__shipping_update", Some(intent)));
        }
        assert!(!relevant(
            "ops__phone_call",
            Some("Explain callbacks in code"),
        ));
    }
    #[test]
    fn intent_work_is_bounded_at_unicode_boundaries() {
        let large = "😀".repeat(10_000);
        let selected = owner_intent(["Continue", large.as_str()].into_iter());
        assert!(selected.len() <= 8192 + "\nContinue".len());
        assert!(selected.ends_with("\nContinue"));
    }
    #[test]
    fn explicit_followup_retains_owner_task_but_new_question_does_not() {
        let selected = owner_intent(["Please continue", "Fix the code and run tests"].into_iter());
        assert!(relevant("shell", Some(&selected)));
        let unrelated =
            owner_intent(["Explain how trees work", "Send a Telegram message"].into_iter());
        assert!(!relevant("ops__telegram_send", Some(&unrelated)));
        assert!(!relevant("shell", Some(&owner_intent(std::iter::empty())),));
    }
}
