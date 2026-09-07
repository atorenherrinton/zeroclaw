//! Model-visible output budgeting. Does not create unretained copies of private
//! connector output in the user attachment store.

pub const ROUND_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_CALLS: usize = 128;

pub fn per_result_budget(configured: usize, calls: usize) -> usize {
    let source_limit = if configured == 0 { 32768 } else { configured };
    source_limit
        .min(ROUND_PAYLOAD_BYTES / calls.max(1))
        .max(256)
}

/// Large payloads retain a bounded excerpt. Automatic resource persistence is
/// deliberately disabled until resources have scope, expiry and reference ownership.
/// HMAC receipts are appended separately by the canonical runtime collector.
/// An excerpt must never be used as proof that an external write did not happen.
pub fn bound_output(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_owned();
    }
    // Overflow must use the existing resource writer once scope, expiry and
    // reference ownership are enforced there. Never create a competing store.
    bounded_excerpt(output, max_bytes)
}

fn bounded_excerpt(output: &str, max_bytes: usize) -> String {
    let evidence = serde_json::from_str::<serde_json::Value>(output)
        .ok()
        .and_then(|v| {
            let object = v.as_object()?;
            let evidence: serde_json::Map<_, _> = [
                "state",
                "status",
                "outcome",
                "retry_allowed",
                "request_id",
                "operation_id",
                "message_id",
                "receipt",
            ]
            .into_iter()
            .filter_map(|key| {
                object
                    .get(key)
                    .filter(|value| serde_json::to_vec(value).is_ok_and(|b| b.len() <= 256))
                    .map(|value| (key.to_owned(), value.clone()))
            })
            .collect();
            Some(serde_json::Value::Object(evidence).to_string())
        })
        .unwrap_or_default();
    let mut footer = format!(
        "\n[truncated output: {} bytes; excerpt is NOT non-delivery evidence; do not replay writes]",
        output.len()
    );
    footer.push_str("\n[full output resource unavailable; narrow the source query]");
    if !evidence.is_empty() && evidence.len() + footer.len() < max_bytes / 2 {
        footer.push_str(&format!("\n[untrusted source evidence: {evidence}]"));
    }
    if footer.len() >= max_bytes {
        // A long workspace path must not blow the model budget. Never return a
        // partial path that could be mistaken for a real retrievable resource.
        footer = "\n[output truncated; full evidence unavailable here; never replay external writes from this excerpt]".into();
    }
    if footer.len() > max_bytes {
        return footer[..footer.floor_char_boundary(max_bytes)].to_owned();
    }
    let keep = max_bytes.saturating_sub(footer.len());
    let end = output.floor_char_boundary(keep);
    format!("{}{footer}", &output[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_round_budget_does_not_persist_unretained_private_data() {
        let output = "😀".repeat(10000);
        let budget = per_result_budget(0, 16);
        let result = bound_output(&output, budget);
        assert!(result.len() <= budget);
        assert!(result.contains("full output resource unavailable"));
        assert!(16 * budget <= ROUND_PAYLOAD_BYTES);
    }
    #[test]
    fn outcome_and_request_id_survive_large_json_result() {
        let output=serde_json::json!({"state":"uncertain","retry_allowed":false,"request_id":"fixture","body":"x".repeat(10000)}).to_string();
        let excerpt = bounded_excerpt(&output, 2048);
        assert!(excerpt.len() <= 2048);
        assert!(excerpt.contains("\"state\":\"uncertain\""));
        assert!(excerpt.contains("\"request_id\":\"fixture\""));
    }
}
