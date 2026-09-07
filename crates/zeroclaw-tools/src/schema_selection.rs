//! Presentation-only initial-turn selection. Execution policy remains the
//! canonical security gate. Unknown intent can always use tool_search.
/// Keep core and discovery schemas; omit unrelated connector schemas only when
/// a discovery tool is present. Do not use server-authored descriptions as policy.
pub fn relevant(name: &str, intent: Option<&str>, discovery_available: bool) -> bool {
    let Some(intent) = intent.filter(|_| discovery_available) else {
        return true;
    };
    if !name.contains("__") {
        return true;
    }
    let intent = intent.to_lowercase();
    let name = name.to_lowercase();
    if intent.contains(&name) {
        return true;
    }
    let groups: &[(&[&str], &[&str])] = &[
        (
            &["calendar", "event"],
            &[
                "calendar",
                "meeting",
                "appointment",
                "availability",
                "schedule",
            ],
        ),
        (
            &["imessage", "message", "telegram"],
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
            &["github", "repository", "pull request", "commit", "code"],
        ),
        (
            &["browser", "safari"],
            &["browser", "safari", "website", "webpage"],
        ),
        (&["phone", "call"], &["phone", "call", "voicemail"]),
        (&["reminder"], &["reminder", "reminders", "todo"]),
        (
            &["shipping", "tracking"],
            &["shipping", "package", "tracking"],
        ),
        (&["genealogy"], &["genealogy", "ancestry", "family tree"]),
    ];
    groups.iter().any(|(names, words)| {
        names.iter().any(|token| {
            name.split(|c: char| !c.is_alphanumeric())
                .any(|part| part == *token)
        }) && words.iter().any(|word| intent.contains(word))
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explanation_omits_unrelated_connectors_and_retains_discovery() {
        for tool in [
            "ops__calendar_events",
            "ops__phone_call",
            "ops__github_pr",
            "ops__genealogy_lookup",
        ] {
            assert!(!relevant(
                tool,
                Some("Explain how a hash table works"),
                true
            ));
        }
        assert!(relevant(
            "tool_search",
            Some("Explain how a hash table works"),
            true
        ));
        assert!(relevant(
            "ops__calendar_events",
            Some("Find tomorrow's meetings"),
            true
        ));
        assert!(!relevant(
            "ops__phone_call",
            Some("Find tomorrow's meetings"),
            true
        ));
        assert!(relevant("ops__unknown", Some("ops__unknown"), true));
        assert!(
            relevant("ops__unknown", Some("explain"), false),
            "never hide tools with no discovery path"
        );
        assert!(
            relevant("ops__unknown", None, true),
            "follow-up activation must remain callable"
        );
    }
}
