//! Bounded offline projection of existing logs, never an action or model executor.
pub mod files;
pub mod report;
use anyhow::{Result, ensure};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use std::collections::{BTreeMap, BTreeSet};

pub const VERSION: u32 = 1;
pub const MAX_LINES: usize = 100_000;
pub const MAX_LINE: usize = 256 * 1024;
pub const MAX_TURNS: usize = 10_000;
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
const TOOL: &str = "typesafe__typesafe_system_one";

/// Domain-separated private-salt identity; never hash a raw identifier unkeyed.
pub fn token(key: &[u8], domain: &str, input: &str) -> Result<String> {
    ensure!(key.len() == 32, "invalid_key_length");
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(b"zeroclaw-typesafe-eval-v1\0");
    mac.update(domain.as_bytes());
    mac.update(b"\0");
    mac.update(input.as_bytes());
    Ok(mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub lines: usize,
    pub ignored: usize,
    pub uncorrelated: usize,
    pub duplicate_events: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub pair_id: String,
    /// Stable analysis partition only. Does not route live traffic.
    pub analysis_partition: u8,
    pub inbound_seen: bool,
    pub generated_ms: Option<u64>,
    pub completion_ms: Option<u64>,
    pub acknowledged_ms: Option<u64>,
    pub generation_to_ack_ms: Option<u64>,
    pub jev_tool_ms: Vec<Option<u64>>,
    pub jev_errors: usize,
    pub jev_unknown_outcomes: usize,
    pub errors: usize,
    pub timeouts: usize,
    pub cancellations: usize,
    pub delivery_confirmed: bool,
    pub ambiguous: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataset {
    pub schema_version: u32,
    pub experiment_id: String,
    pub counts: Counts,
    pub turns: Vec<Turn>,
}

fn bounded_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 256
}
fn duration(v: &Value) -> Result<Option<u64>> {
    match v.pointer("/zeroclaw/duration_ms") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            let n = v.as_u64().filter(|n| *n <= 86_400_000);
            ensure!(n.is_some(), "invalid_duration");
            Ok(n)
        }
    }
}

/// Reads only allowlisted fields from the canonical JSONL. No payload is retained.
/// A malformed or over-bound input aborts the whole collection, never truncates.
pub fn collect(lines: impl Iterator<Item = Result<String>>, key: &[u8]) -> Result<Dataset> {
    let mut counts = Counts::default();
    let mut turns = BTreeMap::<String, Turn>::new();
    let mut seen = BTreeMap::<String, String>::new();
    let mut milestones = BTreeSet::new();
    let mut bytes = 0_u64;
    for line in lines {
        let line = line?;
        counts.lines += 1;
        bytes += line.len() as u64;
        ensure!(
            counts.lines <= MAX_LINES && bytes <= MAX_BYTES && line.len() <= MAX_LINE,
            "collection_limit"
        );
        let v: Value =
            serde_json::from_str(&line).map_err(|_| anyhow::Error::msg("invalid_json_line"))?;
        let channel = v
            .pointer("/zeroclaw/channel_type")
            .and_then(Value::as_str)
            .or_else(|| {
                v.pointer("/zeroclaw/channel")
                    .and_then(Value::as_str)
                    .map(|s| s.split('.').next().unwrap_or(s))
            });
        let message = v.get("message").and_then(Value::as_str).unwrap_or("");
        let action = v
            .pointer("/event/action")
            .and_then(Value::as_str)
            .unwrap_or("");
        let tool = v
            .pointer("/attributes/tool")
            .and_then(Value::as_str)
            .or_else(|| v.pointer("/zeroclaw/tool").and_then(Value::as_str));
        let relevant = matches!(
            (message, action),
            ("channel inbound message", "inbound")
                | ("channel_response_generated", "note")
                | (
                    "Channel final submission completed; consult per-chunk receipts for platform confirmation",
                    "outbound"
                )
                | ("channel_message_error", "fail")
                | ("channel_message_timeout", "timeout")
                | ("channel_message_cancelled", "cancel")
        ) || (message == "tool_call_result"
            && action == "complete"
            && tool == Some(TOOL));
        if channel != Some("telegram") || !relevant {
            counts.ignored += 1;
            continue;
        }
        let ids: Vec<&str> = [
            v.get("trace_id"),
            v.pointer("/attributes/trace_id"),
            v.pointer("/attributes/turn_id"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
        let Some(id) = ids.first().copied().filter(|id| bounded_id(id)) else {
            counts.uncorrelated += 1;
            continue;
        };
        ensure!(ids.iter().all(|s| *s == id), "conflicting_trace_ids");
        let pair_id = token(key, "pair", id)?;
        ensure!(
            turns.contains_key(&pair_id) || turns.len() < MAX_TURNS,
            "turn_limit"
        );
        let t = turns.entry(pair_id.clone()).or_insert_with(|| Turn {
            analysis_partition: u8::from(pair_id.as_bytes()[0] >= b'8'),
            pair_id,
            ..Turn::default()
        });
        // Event UUID is mandatory for deduplication. Never infer identities from time proximity.
        let Some(event_id) = v
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| bounded_id(s))
        else {
            t.ambiguous = true;
            counts.uncorrelated += 1;
            continue;
        };
        let event_key = token(key, "event", event_id)?;
        let fingerprint = token(key, "event-content", &serde_json::to_string(&v)?)?;
        if let Some(previous) = seen.get(&event_key) {
            if previous == &fingerprint {
                counts.duplicate_events += 1;
            } else {
                anyhow::bail!("conflicting_event_id");
            }
            continue;
        }
        seen.insert(event_key, fingerprint);
        let ms = duration(&v)?;
        if message != "tool_call_result"
            && !milestones.insert((t.pair_id.clone(), message.to_owned()))
        {
            t.ambiguous = true;
        }
        match message {
            "channel inbound message" => t.inbound_seen = true,
            "channel_response_generated" => t.generated_ms = ms,
            "Channel final submission completed; consult per-chunk receipts for platform confirmation" =>
            {
                t.completion_ms = ms;
                let d = &v["attributes"]["delivery"];
                let total = d["total_chunks"].as_u64();
                t.delivery_confirmed = v["attributes"]["submission_ok"] == true
                    && d["outcome"] == "confirmed"
                    && total.is_some_and(|n| n > 0)
                    && d["confirmed_chunks"].as_u64() == total;
                if t.delivery_confirmed {
                    t.acknowledged_ms = ms;
                }
            }
            "tool_call_result" => {
                ensure!(t.jev_tool_ms.len() < 128, "tool_call_limit");
                t.jev_tool_ms.push(ms);
                match v["event"]["outcome"].as_str() {
                    Some("success") => {}
                    Some("failure") => t.jev_errors += 1,
                    _ => t.jev_unknown_outcomes += 1,
                }
            }
            "channel_message_error" => t.errors += 1,
            "channel_message_timeout" => t.timeouts += 1,
            "channel_message_cancelled" => t.cancellations += 1,
            _ => unreachable!("relevance filter limits messages"),
        }
        ensure!(turns.len() <= MAX_TURNS, "turn_limit");
    }
    for t in turns.values_mut() {
        if let (Some(start), Some(end)) = (t.generated_ms, t.completion_ms)
            && end < start
        {
            t.ambiguous = true;
        }
        if t.ambiguous {
            t.generated_ms = None;
            t.completion_ms = None;
            t.acknowledged_ms = None;
            t.delivery_confirmed = false;
            t.jev_tool_ms.fill(None);
        }
        t.generation_to_ack_ms = t
            .acknowledged_ms
            .zip(t.generated_ms)
            .and_then(|(end, start)| end.checked_sub(start));
    }
    Ok(Dataset {
        schema_version: VERSION,
        experiment_id: token(key, "experiment", "")?,
        counts,
        turns: turns.into_values().collect(),
    })
}
