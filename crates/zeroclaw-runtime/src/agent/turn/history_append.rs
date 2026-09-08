//! History append for one tool round: the assistant message plus per-call
//! `role=tool` messages (native) or a `[Tool results]` user message (prompt
//! mode).

use zeroclaw_providers::{ChatMessage, ToolCall};

/// Canonical native-result envelope used for both admission measurement and
/// history append. Keeping serialization here prevents the budget from drifting
/// from the provider-history representation.
pub(crate) fn native_tool_result_content(tool_call_id: Option<&str>, result: &str) -> String {
    serde_json::json!({
        "tool_call_id": tool_call_id,
        "content": result,
    })
    .to_string()
}

pub(crate) fn append_tool_round_to_history(
    history: &mut Vec<ChatMessage>,
    assistant_history_content: String,
    native_tool_calls: &[ToolCall],
    individual_results: &[(Option<String>, String)],
    tool_results: &str,
    use_native_tools: bool,
) {
    history.push(ChatMessage::assistant(assistant_history_content));
    if native_tool_calls.is_empty() {
        let all_results_have_ids = use_native_tools
            && !individual_results.is_empty()
            && individual_results
                .iter()
                .all(|(tool_call_id, _)| tool_call_id.is_some());
        if all_results_have_ids {
            for (tool_call_id, result) in individual_results {
                history.push(ChatMessage::tool(native_tool_result_content(
                    tool_call_id.as_deref(),
                    result,
                )));
            }
        } else {
            history.push(ChatMessage::user(format!("[Tool results]\n{tool_results}")));
        }
    } else {
        // `zip` would drop trailing results on any length divergence,
        // leaving a native tool_use id with no matching tool_result.
        // Pair on each result's own id instead.
        for (idx, (tool_call_id, result)) in individual_results.iter().enumerate() {
            let resolved_id = tool_call_id
                .clone()
                .or_else(|| native_tool_calls.get(idx).map(|call| call.id.clone()));
            history.push(ChatMessage::tool(native_tool_result_content(
                resolved_id.as_deref(),
                result,
            )));
        }
    }
}
