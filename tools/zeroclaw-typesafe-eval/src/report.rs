use crate::{Dataset, MAX_TURNS, VERSION};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairs {
    pub schema_version: u32,
    pub experiment_id: String,
    /// Owner-precommitted mapping from decision codes to meanings. One per file.
    pub protocol_version: u32,
    pub pairs: Vec<Pair>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pair {
    pub pair_id: String,
    pub baseline: Decision,
    pub treatment: Decision,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub status: Status,
    pub code: Option<u8>,
}
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Missing,
    Error,
    Timeout,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Labels {
    pub schema_version: u32,
    pub experiment_id: String,
    pub protocol_version: u32,
    /// Only independent gold decision labels, never model self-grades.
    pub rubric_version: u32,
    pub labels: Vec<Label>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub pair_id: String,
    pub gold_code: u8,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub n: usize,
    pub missing: usize,
    pub median_ms: Option<f64>,
    pub p90_ms: Option<u64>,
    pub p95_ms: Option<u64>,
}
pub fn stats(values: impl Iterator<Item = Option<u64>>) -> Stats {
    let all: Vec<_> = values.collect();
    let mut v: Vec<_> = all.iter().flatten().copied().collect();
    v.sort_unstable();
    let n = v.len();
    let median = if n == 0 {
        None
    } else if n % 2 == 1 {
        Some(v[n / 2] as f64)
    } else {
        Some((v[n / 2 - 1] as f64 + v[n / 2] as f64) / 2.0)
    };
    let percentile = |p: usize| {
        if n == 0 {
            None
        } else {
            Some(v[(n * p).div_ceil(100) - 1])
        }
    };
    Stats {
        n,
        missing: all.len() - n,
        median_ms: median,
        p90_ms: percentile(90),
        p95_ms: percentile(95),
    }
}
fn hex_id(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

pub fn report(data: &Dataset, pairs: Option<Pairs>, labels: Option<Labels>) -> Result<Value> {
    ensure!(
        data.schema_version == VERSION
            && hex_id(&data.experiment_id)
            && data.turns.len() <= MAX_TURNS,
        "invalid_dataset"
    );
    let ids: BTreeSet<_> = data.turns.iter().map(|t| t.pair_id.as_str()).collect();
    ensure!(
        ids.len() == data.turns.len() && ids.iter().all(|id| hex_id(id)),
        "invalid_pair_ids"
    );
    ensure!(
        data.counts.lines <= crate::MAX_LINES
            && data
                .counts
                .ignored
                .saturating_add(data.counts.uncorrelated)
                .saturating_add(data.counts.duplicate_events)
                <= data.counts.lines,
        "invalid_source_counts"
    );
    for t in &data.turns {
        ensure!(
            t.analysis_partition == u8::from(t.pair_id.as_bytes()[0] >= b'8')
                && t.jev_tool_ms.len() <= 128
                && t.jev_errors.saturating_add(t.jev_unknown_outcomes) <= t.jev_tool_ms.len()
                && [t.errors, t.timeouts, t.cancellations]
                    .iter()
                    .all(|c| *c <= data.counts.lines),
            "invalid_turn_counts"
        );
        let durations = [
            t.generated_ms,
            t.completion_ms,
            t.acknowledged_ms,
            t.generation_to_ack_ms,
        ];
        ensure!(
            durations
                .iter()
                .chain(t.jev_tool_ms.iter())
                .flatten()
                .all(|ms| *ms <= 86_400_000)
                && (!t.ambiguous
                    || (durations
                        .iter()
                        .chain(t.jev_tool_ms.iter())
                        .all(Option::is_none)
                        && !t.delivery_confirmed))
                && (t.acknowledged_ms.is_none()
                    || (t.delivery_confirmed && t.acknowledged_ms == t.completion_ms))
                && t.generated_ms
                    .zip(t.completion_ms)
                    .is_none_or(|(start, end)| start <= end)
                && t.generation_to_ack_ms
                    == t.acknowledged_ms
                        .zip(t.generated_ms)
                        .and_then(|(end, start)| end.checked_sub(start)),
            "invalid_turn_timings"
        );
    }
    let n = data.turns.len();
    let mut paired = 0;
    let mut changed = 0;
    let mut labeled = 0;
    let mut useful = 0;
    let mut harmful = 0;
    let mut labeled_changes = 0;
    let mut baseline_correct = 0;
    let mut treatment_correct = 0;
    let mut baseline_errors = 0;
    let mut treatment_errors = 0;
    let mut baseline_timeouts = 0;
    let mut treatment_timeouts = 0;
    let mut supplied = 0;
    ensure!(pairs.is_some() || labels.is_none(), "labels_require_pairs");
    if let Some(p) = pairs {
        ensure!(
            p.schema_version == VERSION
                && p.experiment_id == data.experiment_id
                && p.protocol_version > 0
                && p.pairs.len() <= MAX_TURNS,
            "pair_protocol_mismatch"
        );
        let mut gold = BTreeMap::new();
        if let Some(l) = labels {
            ensure!(
                l.schema_version == VERSION
                    && l.experiment_id == data.experiment_id
                    && l.protocol_version == p.protocol_version
                    && l.rubric_version == 1
                    && l.labels.len() <= MAX_TURNS,
                "label_protocol_mismatch"
            );
            for label in l.labels {
                ensure!(
                    label.gold_code < 64 && gold.insert(label.pair_id, label.gold_code).is_none(),
                    "invalid_or_duplicate_label"
                );
            }
        }
        let mut seen = BTreeSet::new();
        for pair in p.pairs {
            ensure!(
                ids.contains(pair.pair_id.as_str()) && seen.insert(pair.pair_id.clone()),
                "unmatched_or_duplicate_pair"
            );
            let turn = data
                .turns
                .iter()
                .find(|t| t.pair_id == pair.pair_id)
                .ok_or_else(|| anyhow::Error::msg("unmatched_pair"))?;
            ensure!(!turn.ambiguous, "ambiguous_pair");
            for d in [&pair.baseline, &pair.treatment] {
                ensure!(
                    (d.status == Status::Ok) == d.code.is_some() && d.code.is_none_or(|c| c < 64),
                    "invalid_decision"
                );
            }
            supplied += 1;
            baseline_errors += usize::from(pair.baseline.status == Status::Error);
            treatment_errors += usize::from(pair.treatment.status == Status::Error);
            baseline_timeouts += usize::from(pair.baseline.status == Status::Timeout);
            treatment_timeouts += usize::from(pair.treatment.status == Status::Timeout);
            let label = gold.remove(&pair.pair_id);
            if let (Some(b), Some(t)) = (pair.baseline.code, pair.treatment.code) {
                paired += 1;
                changed += usize::from(b != t);
                if let Some(g) = label {
                    labeled += 1;
                    baseline_correct += usize::from(b == g);
                    treatment_correct += usize::from(t == g);
                    labeled_changes += usize::from(b != t);
                    useful += usize::from(b != g && t == g);
                    harmful += usize::from(b == g && t != g);
                }
            } else {
                ensure!(label.is_none(), "label_on_incomplete_pair");
            }
        }
        ensure!(gold.is_empty(), "unmatched_labels");
    }
    let decisions = json!({ "supplied": supplied, "unsupplied": n - supplied,
        "complete_pairs": paired, "incomplete_supplied_pairs": supplied - paired,
        "changed": changed, "unchanged": paired - changed,
        "change_rate": if paired > 0 { Some(changed as f64 / paired as f64) } else { None },
        "baseline_errors": baseline_errors, "treatment_errors": treatment_errors,
        "baseline_timeouts": baseline_timeouts, "treatment_timeouts": treatment_timeouts,
        "gold_labeled_pairs": labeled, "unlabeled_complete_pairs": paired - labeled,
        "gold_labeled_changes": labeled_changes,
        "label_coverage": if paired > 0 { Some(labeled as f64 / paired as f64) } else { None },
        "useful_change_rate_among_labeled_changes": if labeled_changes > 0 { Some(useful as f64 / labeled_changes as f64) } else { None },
        "harmful_change_rate_among_labeled_changes": if labeled_changes > 0 { Some(harmful as f64 / labeled_changes as f64) } else { None },
        "useful_change_rate_among_labeled_pairs": if labeled > 0 { Some(useful as f64 / labeled as f64) } else { None },
        "harmful_change_rate_among_labeled_pairs": if labeled > 0 { Some(harmful as f64 / labeled as f64) } else { None },
        "useful_decision_changes": if labeled > 0 { Some(useful) } else { None },
        "harmful_decision_changes": if labeled > 0 { Some(harmful) } else { None },
        "baseline_gold_correct": if labeled > 0 { Some(baseline_correct) } else { None },
        "treatment_gold_correct": if labeled > 0 { Some(treatment_correct) } else { None }
    });
    let turns = &data.turns;
    Ok(json!({
        "schema_version": VERSION, "experiment_id": data.experiment_id,
        "mode": "offline_observational", "assignment_version": 1,
        "causal_ab": false, "response_quality_evidence": "insufficient",
        "source_counts": data.counts, "observed_turns": n,
        "analysis_partitions": [turns.iter().filter(|t| t.analysis_partition == 0).count(), turns.iter().filter(|t| t.analysis_partition == 1).count()],
        "inbound_missing": turns.iter().filter(|t| !t.inbound_seen).count(),
        "ambiguous_turns": turns.iter().filter(|t| t.ambiguous).count(),
        "generation_errors": turns.iter().map(|t| t.errors).sum::<usize>(),
        "generation_timeouts": turns.iter().map(|t| t.timeouts).sum::<usize>(),
        "cancellations": turns.iter().map(|t| t.cancellations).sum::<usize>(),
        "delivery_confirmed": turns.iter().filter(|t| t.delivery_confirmed).count(),
        "delivery_unconfirmed_or_missing": turns.iter().filter(|t| !t.delivery_confirmed).count(),
        "turns_with_jev": turns.iter().filter(|t| !t.jev_tool_ms.is_empty()).count(),
        "jev_tool_errors": turns.iter().map(|t| t.jev_errors).sum::<usize>(),
        "jev_unknown_outcomes": turns.iter().map(|t| t.jev_unknown_outcomes).sum::<usize>(),
        "jev_timeout_count": null,
        "timings": {
            "processing_to_generated": stats(turns.iter().map(|t| t.generated_ms)),
            "processing_to_submission_completion": stats(turns.iter().map(|t| t.completion_ms)),
            "processing_to_confirmed_ack": stats(turns.iter().map(|t| t.acknowledged_ms)),
            "generated_to_confirmed_ack": stats(turns.iter().map(|t| t.generation_to_ack_ms)),
            "jev_tool_call": stats(turns.iter().flat_map(|t| t.jev_tool_ms.iter().copied())),
            "platform_to_receipt": stats(turns.iter().map(|_| None)),
            "receipt_to_processing": stats(turns.iter().map(|_| None)),
            "platform_to_confirmed_ack": stats(turns.iter().map(|_| None))
        },
        "decisions": decisions
    }))
}
