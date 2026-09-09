//! Model-visible output budgeting. Does not create unretained copies of private
//! connector output in the user attachment store.

pub const ROUND_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_CALLS: usize = 128;

pub use zeroclaw_api::serialization::encoded_size;

tokio::task_local! {
    // Derived from the current execution round, never cached across turns.
    static ROUND_PREVIEW_LIMIT: usize;
}

/// Reserve space for source mirrors, native JSON escaping, receipts, and errors.
/// The canonical runtime admission checks remain authoritative; this is only a
/// presentation allowance for tools that already opt into previewing.
pub async fn with_round_preview_budget<F: std::future::Future>(
    configured_limit: usize,
    calls: usize,
    future: F,
) -> F::Output {
    let configured = if configured_limit == 0 {
        32768
    } else {
        configured_limit
    };
    let share = configured.min(ROUND_PAYLOAD_BYTES / calls.max(1));
    let limit = share.saturating_sub(1024) / 8;
    ROUND_PREVIEW_LIMIT
        .scope(limit.clamp(384, READ_PREVIEW_BYTES), future)
        .await
}

pub fn preview_limit(max_bytes: usize) -> usize {
    ROUND_PREVIEW_LIMIT
        .try_with(|limit| max_bytes.min(*limit))
        .unwrap_or(max_bytes)
}

/// Format a preview at its tool boundary, before display and structured
/// mirrors are constructed. The caller supplies the localized omission notice.
/// This does not admit the result to history or establish external-effect safety.
pub fn bounded_text_preview(output: String, max_bytes: usize, marker: &str) -> String {
    let max_bytes = preview_limit(max_bytes);
    if encoded_size(&output, max_bytes).is_some() {
        return output;
    }
    if encoded_size(&marker, max_bytes).is_none() {
        // Never cut a warning into an ambiguous result. Runtime admission still
        // rejects an oversized projection when the localized notice cannot fit.
        return output;
    }
    let excerpt = |bytes: usize| {
        let head = output.floor_char_boundary(bytes * 3 / 4);
        let tail = output.ceil_char_boundary(output.len().saturating_sub(bytes / 4));
        format!("{}{}{}", &output[..head], marker, &output[tail..])
    };
    let (mut low, mut high) = (0, max_bytes.min(output.len()));
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if encoded_size(&excerpt(mid), max_bytes).is_some() {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    excerpt(low)
}

/// Presentation allowance for text-only reads. Runtime admission still owns
/// complete source/history envelopes and the aggregate batch ceiling.
pub const READ_PREVIEW_BYTES: usize = 4096;

pub fn read_text_preview(output: String) -> String {
    if encoded_size(&output, preview_limit(READ_PREVIEW_BYTES)).is_some() {
        return output;
    }
    let marker = format!(
        "\n\n{}\n\n",
        crate::i18n::get_required_tool_string("read-result-truncated")
    );
    bounded_text_preview(output, READ_PREVIEW_BYTES, &marker)
}

/// Only callers that own a read-only, text presentation contract may use this.
/// Preserve typed data and failures; never erase a machine-readable payload or
/// infer that an unknown/mixed operation is safe from its tool name.
pub fn preview_read_result(
    mut result: zeroclaw_api::tool::ToolResult,
) -> zeroclaw_api::tool::ToolResult {
    if result.success && result.output.data().is_none() {
        result.output = read_text_preview(result.output.into_string()).into();
    }
    result
}

/// Exact encodings cannot be previewed: return a small, recoverable read
/// failure rather than partial bytes/JSON falsely labelled as a successful read.
pub fn exact_read_result(result: zeroclaw_api::tool::ToolResult) -> zeroclaw_api::tool::ToolResult {
    if encoded_size(&result, preview_limit(16 * 1024)).is_none() {
        return zeroclaw_api::tool::ToolResult {
            success: false,
            output: Default::default(),
            error: Some(crate::i18n::get_required_tool_string(
                "exact-read-result-too-large",
            )),
        };
    }
    result
}

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
                "occurrence_id",
                "job_id",
                "duplicate",
                "effect_outcome",
                "execution_outcome",
                "delivery_outcome",
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
    if !evidence.is_empty() {
        let evidence_footer = format!("\n[untrusted source evidence: {evidence}]");
        if footer.len() + evidence_footer.len() <= max_bytes {
            footer.push_str(&evidence_footer);
        } else {
            // Evidence takes priority over the excerpt, including at the
            // 512-byte per-result budget for the maximum 128-call round.
            let compact = format!(
                "\n[truncated; full output resource unavailable; do not replay writes]{evidence_footer}"
            );
            if compact.len() <= max_bytes {
                footer = compact;
            }
        }
    }
    if footer.len() > max_bytes {
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
    fn preview_preserves_typed_results_and_exact_reads_fail_without_partial_data() {
        use zeroclaw_api::tool::{ToolOutput, ToolResult};
        let typed = ToolResult {
            success: true,
            output: ToolOutput::json(serde_json::json!({"body":"x".repeat(30_000)})),
            error: None,
        };
        let original = serde_json::to_vec(&typed).unwrap();
        assert_eq!(
            serde_json::to_vec(&preview_read_result(typed)).unwrap(),
            original
        );
        for success in [true, false] {
            let exact = exact_read_result(ToolResult {
                success,
                output: "x".repeat(30_000).into(),
                error: Some("details".into()),
            });
            assert!(!exact.success);
            assert!(exact.output.is_empty());
            assert!(exact.error.unwrap().contains("No partial binary or JSON"));
        }
    }

    #[tokio::test]
    async fn round_budget_is_isolated_and_nested_scope_restores_parent() {
        assert_eq!(preview_limit(4096), 4096);
        with_round_preview_budget(32768, 16, async {
            let outer = preview_limit(4096);
            assert!(outer < 4096);
            with_round_preview_budget(32768, 1, async {
                assert!(preview_limit(4096) > outer);
            })
            .await;
            assert_eq!(preview_limit(4096), outer);
            let preview = read_text_preview("\\\"😀".repeat(50_000));
            assert!(encoded_size(&preview, outer).is_some());
        })
        .await;
        assert_eq!(preview_limit(4096), 4096);
    }

    #[test]
    fn text_preview_preserves_small_output_and_warns_without_splitting_unicode() {
        let small = "\0😀\"\\".repeat(20);
        assert_eq!(bounded_text_preview(small.clone(), 4096, "NOTICE"), small);
        let large = format!("START{}END", "\0😀\"\\".repeat(20_000));
        let preview = bounded_text_preview(large.clone(), 4096, "\nNOTICE\n");
        assert!(encoded_size(&preview, 4096).is_some());
        assert!(preview.starts_with("START"));
        assert!(preview.ends_with("END"));
        assert_eq!(preview.matches("NOTICE").count(), 1);
        // An oversized translated warning must fail closed at history admission.
        assert_eq!(
            bounded_text_preview(large.clone(), 32, &"!".repeat(33)),
            large
        );
    }

    #[test]
    fn encoded_counter_matches_serializer_and_stops_at_exact_limit() {
        let value = serde_json::json!({"id": "\u{0001}😀\"\\", "metadata": [null, false, 42]});
        let actual = serde_json::to_vec(&value).unwrap().len();
        assert_eq!(encoded_size(&value, actual), Some(actual));
        assert_eq!(encoded_size(&value, actual - 1), None);
        assert_eq!(encoded_size(&value, 0), None);
    }

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
    fn manual_occurrence_receipt_survives_output_trimming() {
        let id = format!("manual:key:{}", "a".repeat(64));
        let output = serde_json::json!({"body":"😀".repeat(20000),
            "job_id":"00000000-0000-0000-0000-000000000001", "duplicate":true,
            "status":"uncertain", "occurrence_id":id,
            "effect_outcome":"reconciliation_required","execution_outcome":"confirmed",
            "delivery_outcome":"possibly_applied"})
        .to_string();
        let budget = per_result_budget(0, MAX_BATCH_CALLS);
        let bounded = bound_output(&output, budget);
        assert!(bounded.len() <= budget);
        assert!(bounded.len() * MAX_BATCH_CALLS <= ROUND_PAYLOAD_BYTES);
        let evidence = bounded
            .split("[untrusted source evidence: ")
            .nth(1)
            .unwrap();
        let evidence: serde_json::Value =
            serde_json::from_str(evidence.strip_suffix(']').unwrap()).unwrap();
        for key in [
            "job_id",
            "duplicate",
            "status",
            "occurrence_id",
            "effect_outcome",
            "execution_outcome",
            "delivery_outcome",
        ] {
            let original: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(evidence[key], original[key], "{key}");
        }
        assert!(bounded.contains("do not replay writes"));
        let evidence_only_budget = bounded.len() - bounded.find("\n[truncated").unwrap();
        let evidence_only = bound_output(&output, evidence_only_budget);
        assert_eq!(evidence_only.len(), evidence_only_budget);
        assert!(evidence_only.contains(&id));
        assert!(evidence_only.contains("reconciliation_required"));
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
