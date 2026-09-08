//! Model-visible output budgeting. Does not create unretained copies of private
//! connector output in the user attachment store.

pub const ROUND_PAYLOAD_BYTES: usize = 64 * 1024;
pub const MAX_BATCH_CALLS: usize = 128;

/// Count the complete JSON representation without allocating a serialized copy.
/// Stop serialization as soon as the limit is exceeded. Callers must treat
/// serialization failure as over budget, never as a zero-sized payload.
pub fn encoded_size<T: serde::Serialize + ?Sized>(value: &T, limit: usize) -> Option<usize> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(std::io::Error::other("encoded tool output budget exceeded"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.used)
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
