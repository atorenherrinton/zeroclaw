//! Conservative recovery for an initial progress preamble mistaken for a final answer.
//!
//! History owns the request and evidence of tool work. This is not a general
//! completion classifier: uncertain prose stays final, and a turn that has
//! already attempted tools is never replayed because of this heuristic.

use zeroclaw_providers::ChatMessage;

pub(super) const INSTRUCTION: &str = "[Complete the current request]\n\
    Your last response only announced work; no tool work has started for this request. \
    A progress update does not complete it. Continue the user's original request now: \
    use the available tools only when the authorized task requires them, or give the \
    actual answer, a necessary clarification, or a concrete blocker. Do not send another \
    promise to act later. Do not repeat completed actions, invent results, bypass policy, \
    or treat this reminder as new authorization.";

pub(super) fn append_instruction(messages: &mut Vec<ChatMessage>) {
    if let Some(system) = messages.iter_mut().find(|message| message.role == "system") {
        system.content.push_str("\n\n");
        system.content.push_str(INSTRUCTION);
    } else {
        messages.insert(0, ChatMessage::system(INSTRUCTION));
    }
}

pub(super) fn is_initial_progress_only(text: &str, history: &[ChatMessage]) -> bool {
    let checkpoint = crate::i18n::get_required_cli_string("turn-partial-checkpoint");
    let Some(request_index) = history.iter().rposition(|message| {
        message.role == "user"
            && !message.content.starts_with("[Tool results]")
            && !message.content.starts_with("[Tool call parse error]")
            && message.content != checkpoint
    }) else {
        return false;
    };
    // Any tool round, successful or not, may have caused effects. Derive this
    // from canonical history rather than maintaining another execution flag.
    if history[request_index + 1..].iter().any(|message| {
        message.role == "tool"
            || message.content.starts_with("[Tool results]")
            || (message.role == "assistant"
                && serde_json::from_str::<serde_json::Value>(&message.content)
                    .ok()
                    .and_then(|value| value.get("tool_calls").cloned())
                    .and_then(|value| value.as_array().map(|calls| !calls.is_empty()))
                    .unwrap_or(false))
    }) {
        return false;
    }
    let request = history[request_index].content.to_lowercase();
    // A requested plan, quotation, wording exercise, or status report can
    // legitimately consist of first-person future/progressive tense.
    if request.split(|c: char| !c.is_alphabetic()).any(|word| {
        matches!(
            word,
            "quote"
                | "quoted"
                | "repeat"
                | "rewrite"
                | "rephrase"
                | "paraphrase"
                | "translate"
                | "translation"
                | "sentence"
                | "wording"
                | "grammar"
                | "example"
                | "plan"
                | "planning"
                | "strategy"
                | "steps"
                | "status"
        )
    }) || [
        "what are you doing",
        "what will you",
        "how would you",
        "how do i",
        "how to ",
        "respond with ",
        "reply with ",
        "say exactly ",
        "write an update",
        "write a message",
        "draft a ",
    ]
    .iter()
    .any(|phrase| request.contains(phrase))
    {
        return false;
    }

    let text = text.trim().replace('’', "'").to_lowercase();
    if text.is_empty()
        || text.len() > 600
        || text.split_whitespace().count() > 90
        || text.contains(['\n', '?', ':', '`', '"', '“', '”', '[', ']', '/', '='])
        || text.chars().any(|c| c.is_ascii_digit())
    {
        return false;
    }
    let mut action_seen = false;
    for sentence in text
        .split(['.', '!', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let sentence = sentence
            .strip_prefix("first, ")
            .or_else(|| sentence.strip_prefix("first "))
            .unwrap_or(sentence);
        // Every sentence must be a recognizable preamble. One substantive
        // result, clarification, or blocker preserves the ordinary final path.
        let immediate = ["i'm ", "i am "]
            .iter()
            .find_map(|prefix| sentence.strip_prefix(prefix))
            .is_some_and(|rest| starts_with_action(rest, true));
        let future = ["i'll ", "i will "]
            .iter()
            .find_map(|prefix| sentence.strip_prefix(prefix));
        let future_action = future.is_some_and(|rest| starts_with_action(rest, false));
        let followup = future.is_some_and(|rest| {
            [
                "give you ",
                "let you know ",
                "report back ",
                "get back to you ",
            ]
            .iter()
            .any(|prefix| rest.starts_with(prefix))
        });
        let acknowledgment = matches!(
            sentence,
            "i'm here" | "i am here" | "i dropped the follow-through"
        );
        if !immediate && !future_action && !followup && !acknowledgment {
            return false;
        }
        action_seen |= immediate || future_action;
    }
    action_seen
}

fn starts_with_action(text: &str, progressive: bool) -> bool {
    // A narrow allowlist captures the observed failure, not all promises.
    // In particular, waiting/background updates and claims of completed work
    // must not restart execution.
    let verbs: &[&str] = if progressive {
        &[
            "checking",
            "reviewing",
            "retrieving",
            "fetching",
            "inspecting",
            "searching",
            "reading",
            "verifying",
            "testing",
            "investigating",
        ]
    } else {
        &[
            "check",
            "review",
            "retrieve",
            "fetch",
            "inspect",
            "search",
            "read",
            "verify",
            "test",
            "investigate",
        ]
    };
    let specific_action = ["up ", "into ", "at ", "through ", "for "]
        .iter()
        .any(|object| {
            text.strip_prefix(if progressive { "looking " } else { "look " })
                .is_some_and(|tail| tail.starts_with(object))
        })
        || (progressive
            && [
                "doing the review ",
                "doing the actual review ",
                "doing the check ",
                "doing the actual check ",
            ]
            .iter()
            .any(|prefix| text.starts_with(prefix)));
    (specific_action
        || verbs.iter().any(|verb| {
            text.strip_prefix(verb)
                .is_some_and(|tail| tail.starts_with(' '))
        }))
        && !text.starts_with("reading this as ")
        && !text.starts_with("read this as ")
        && ![
            " but ",
            " because ",
            " found ",
            " confirmed ",
            " completed ",
            " finished ",
            " cannot ",
            " can't ",
            " need you ",
            " already ",
        ]
        .iter()
        .any(|signal| text.contains(signal))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_unstarted_action_preambles_request_repair() {
        let history = vec![ChatMessage::user("Please review the forms.")];
        for text in [
            "I’m checking the actual thread now. I’ll give you the reviewed forms and specific callouts, not another placeholder update.",
            "I’m doing the review now. First I’m retrieving the exact email thread.",
            "I'm checking the codebase and background tasks to see where things stand.",
            "I’m here. I dropped the follow-through. I’m doing the actual review now, starting with the attachments.",
        ] {
            assert!(is_initial_progress_only(text, &history), "{text}");
        }
        for text in [
            "The forms are complete.",
            "I'm checking the forms. The signature is missing.",
            "I'm checking the forms but cannot access the attachment.",
            "I'm checking the forms. Which thread should I use?",
            "I'm checking the forms and already found the missing signature.",
            "I'll let you know as soon as there's a verified milestone.",
            "The sentence is: I'm checking the forms.",
            "I'm waiting for your approval.",
            "I'm looking forward to hearing from you.",
            "I'll look forward to it.",
            "I'm doing well, thanks.",
            "I'm reading this as a request for a plan.",
        ] {
            assert!(!is_initial_progress_only(text, &history), "{text}");
        }
        for request in [
            "Rewrite this update in the first person.",
            "Give me a plan for reviewing forms.",
            "What are you doing?",
            "Translate this sentence.",
            "Respond with I'm checking the forms.",
            "Write an update saying you are checking the forms.",
        ] {
            assert!(!is_initial_progress_only(
                "I'm checking the forms.",
                &[ChatMessage::user(request)]
            ));
        }
        for evidence in [
            ChatMessage::tool("result"),
            ChatMessage::user("[Tool results]\nresult"),
            ChatMessage::assistant(r#"{"content":null,"tool_calls":[{"name":"send"}]}"#),
        ] {
            let mut attempted = history.clone();
            attempted.push(evidence);
            assert!(!is_initial_progress_only(
                "I'm checking the forms.",
                &attempted
            ));
            attempted.push(ChatMessage::user(crate::i18n::get_required_cli_string(
                "turn-partial-checkpoint",
            )));
            assert!(
                !is_initial_progress_only("I'm checking the forms.", &attempted),
                "runtime checkpoint must not hide prior execution"
            );
        }
    }
}
