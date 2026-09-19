use serde_json::{Value, json};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::{fs, path::Path, process::Command};
use zeroclaw_typesafe_eval::{collect, files, report, token};

fn event(id: &str, turn: &str, message: &str, action: &str, ms: Option<u64>) -> Value {
    json!({"id":id,"trace_id":turn,"@timestamp":"2026-01-01T00:00:00Z",
        "event":{"action":action,"outcome":"success"},
        "zeroclaw":{"channel":"telegram.test","channel_type":"telegram","duration_ms":ms},
        "message":message,"attributes":{"trace_id":turn}})
}
fn fixture() -> Vec<Value> {
    let mut out = vec![
        event("e1", "turn-a", "channel inbound message", "inbound", None),
        event("e2", "turn-a", "tool_call_result", "complete", Some(20)),
        event(
            "e3",
            "turn-a",
            "channel_response_generated",
            "note",
            Some(100),
        ),
        event(
            "e4",
            "turn-a",
            "Channel final submission completed; consult per-chunk receipts for platform confirmation",
            "outbound",
            Some(140),
        ),
        event("e5", "turn-b", "channel inbound message", "inbound", None),
        event(
            "e6",
            "turn-b",
            "channel_message_timeout",
            "timeout",
            Some(200),
        ),
    ];
    out[1]["attributes"]["tool"] = json!("typesafe__typesafe_system_one");
    out[1]["attributes"]["output"] = json!("PRIVATE_RATIONALE");
    out[0]["attributes"]["sender"] = json!("PRIVATE_SENDER");
    out[3]["attributes"]["submission_ok"] = json!(true);
    out[3]["attributes"]["delivery"] =
        json!({"outcome":"confirmed","confirmed_chunks":2,"total_chunks":2});
    out
}
fn data(events: Vec<Value>) -> zeroclaw_typesafe_eval::Dataset {
    collect(events.into_iter().map(|e| Ok(e.to_string())), &[7; 32]).unwrap()
}
fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_zeroclaw-typesafe-eval"))
        .args(args)
        .output()
        .unwrap()
}
fn private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn real_cli_collect_report_boundary_and_no_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let dir = root.join("experiment");
    let trace = root.join("trace.jsonl");
    let lines = fixture()
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    private(&trace, lines.as_bytes());
    assert!(run(&["init", dir.to_str().unwrap()]).status.success());
    assert_eq!(
        fs::metadata(dir.join("salt")).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        run(&["collect", dir.to_str().unwrap(), trace.to_str().unwrap()])
            .status
            .success()
    );
    assert!(run(&["report", dir.to_str().unwrap()]).status.success());
    let records = fs::read_to_string(dir.join("records.json")).unwrap();
    for raw in ["turn-a", "PRIVATE", "2026-01-01", "telegram.test"] {
        assert!(!records.contains(raw));
    }
    let r: Value = serde_json::from_slice(&fs::read(dir.join("report.json")).unwrap()).unwrap();
    assert_eq!(r["observed_turns"], 2);
    assert_eq!(
        r["timings"]["processing_to_confirmed_ack"]["median_ms"],
        140.0
    );
    assert_eq!(
        r["timings"]["generated_to_confirmed_ack"]["median_ms"],
        40.0
    );
    assert_eq!(r["timings"]["platform_to_confirmed_ack"]["n"], 0);
    assert_eq!(r["timings"]["platform_to_confirmed_ack"]["missing"], 2);
    assert_eq!(r["generation_timeouts"], 1);
    assert!(r["decisions"]["useful_decision_changes"].is_null());
    assert_eq!(r["response_quality_evidence"], "insufficient");
    assert!(
        !run(&["collect", dir.to_str().unwrap(), trace.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!run(&["report", dir.to_str().unwrap()]).status.success());
}

#[test]
fn stable_private_assignment_order_and_duplicate_handling() {
    let original = data(fixture());
    let mut reversed = fixture();
    reversed.reverse();
    reversed.push(reversed[0].clone());
    let duplicate = data(reversed);
    assert_eq!(
        serde_json::to_value(original.turns).unwrap(),
        serde_json::to_value(duplicate.turns).unwrap()
    );
    assert_eq!(duplicate.counts.duplicate_events, 1);
    assert_ne!(
        token(&[7; 32], "pair", "turn-a").unwrap(),
        token(&[8; 32], "pair", "turn-a").unwrap()
    );
    assert_ne!(
        token(&[7; 32], "pair", "turn-a").unwrap(),
        token(&[7; 32], "event", "turn-a").unwrap()
    );
    // Pinned HMAC vector protects assignment version against implementation drift.
    assert_eq!(
        token(&[7; 32], "pair", "turn-a").unwrap(),
        "44fa64fd7f61c12684269fc57aa34f41e68c49e16c76df202d8df70b2c8f9cdf"
    );
}

#[test]
fn missing_and_ambiguous_timings_never_become_zero_or_acknowledgement() {
    let mut f = fixture();
    f[3]["attributes"]["delivery"]["outcome"] = json!("possibly_applied");
    f[1]["zeroclaw"]["duration_ms"] = Value::Null;
    let d = data(f.clone());
    let t = d
        .turns
        .iter()
        .find(|t| t.inbound_seen && t.generated_ms.is_some())
        .unwrap();
    assert!(t.acknowledged_ms.is_none());
    assert_eq!(t.jev_tool_ms, vec![None]);
    f[3]["zeroclaw"]["duration_ms"] = json!(99);
    let d = data(f);
    assert_eq!(d.turns.iter().filter(|t| t.ambiguous).count(), 1);
    assert!(d.turns.iter().all(|t| t.generated_ms.is_none()));
    let mut f = fixture();
    let mut second = f[2].clone();
    second["id"] = json!("different-event");
    f.push(second);
    assert_eq!(data(f).turns.iter().filter(|t| t.ambiguous).count(), 1);
}

#[test]
fn invalid_conflicting_and_uncorrelated_sources() {
    let mut f = fixture();
    let mut conflict = f[0].clone();
    conflict["attributes"]["sender"] = json!("different");
    f.push(conflict);
    assert!(collect(f.into_iter().map(|v| Ok(v.to_string())), &[7; 32]).is_err());
    assert!(collect([Ok("{invalid PRIVATE".into())].into_iter(), &[7; 32]).is_err());
    let mut f = fixture();
    f[0]["trace_id"] = json!("other-turn");
    assert!(collect(f.into_iter().map(|v| Ok(v.to_string())), &[7; 32]).is_err());
    let mut f = fixture();
    f[0].as_object_mut().unwrap().remove("trace_id");
    f[0]["attributes"] = json!({});
    assert_eq!(data(f).counts.uncorrelated, 1);
}

#[test]
fn labels_are_matched_independent_decision_evidence_only() {
    let d = data(fixture());
    let id = d.turns[0].pair_id.clone();
    let p = json!({"schema_version":1,"experiment_id":d.experiment_id,"protocol_version":1,
        "pairs":[{"pair_id":id,"baseline":{"status":"ok","code":0},"treatment":{"status":"ok","code":1}}]});
    let l = json!({"schema_version":1,"experiment_id":d.experiment_id,"protocol_version":1,"rubric_version":1,
        "labels":[{"pair_id":id,"gold_code":1}]});
    let r = report::report(&d, Some(serde_json::from_value(p.clone()).unwrap()), None).unwrap();
    assert_eq!(r["decisions"]["changed"], 1);
    assert!(r["decisions"]["useful_decision_changes"].is_null());
    let r = report::report(
        &d,
        Some(serde_json::from_value(p.clone()).unwrap()),
        Some(serde_json::from_value(l.clone()).unwrap()),
    )
    .unwrap();
    assert_eq!(r["decisions"]["useful_decision_changes"], 1);
    assert_eq!(r["response_quality_evidence"], "insufficient");
    let mut duplicate = p.clone();
    duplicate["pairs"]
        .as_array_mut()
        .unwrap()
        .push(p["pairs"][0].clone());
    assert!(report::report(&d, Some(serde_json::from_value(duplicate).unwrap()), None).is_err());
    let mut wrong = l;
    wrong["protocol_version"] = json!(2);
    assert!(
        report::report(
            &d,
            Some(serde_json::from_value(p).unwrap()),
            Some(serde_json::from_value(wrong).unwrap())
        )
        .is_err()
    );
}

#[test]
fn quantiles_denominators_and_empty_samples() {
    let s = report::stats([Some(10), Some(20), Some(30), Some(40), None].into_iter());
    assert_eq!(s.n, 4);
    assert_eq!(s.missing, 1);
    assert_eq!(s.median_ms, Some(25.0));
    assert_eq!(s.p90_ms, Some(40));
    assert_eq!(s.p95_ms, Some(40));
    let s = report::stats([None].into_iter());
    assert!(s.median_ms.is_none());
    assert!(s.p90_ms.is_none());
}

#[test]
fn report_rejects_mutated_projection_fields() {
    let mut d = data(fixture());
    d.turns[0].analysis_partition = 9;
    assert!(report::report(&d, None, None).is_err());
    let mut d = data(fixture());
    d.turns[0].generated_ms = Some(86_400_001);
    assert!(report::report(&d, None, None).is_err());
    let mut d = data(fixture());
    d.turns[0].jev_errors = usize::MAX;
    assert!(report::report(&d, None, None).is_err());
    let mut d = data(fixture());
    d.counts.ignored = usize::MAX;
    assert!(report::report(&d, None, None).is_err());
    let mut d = data(fixture());
    d.turns[0].generation_to_ack_ms = Some(1);
    assert!(report::report(&d, None, None).is_err());
}

#[test]
fn missing_event_ids_cannot_bypass_turn_limit() {
    let lines = (0..=zeroclaw_typesafe_eval::MAX_TURNS).map(|i| {
        let mut v = event(
            "unused",
            &format!("turn-{i}"),
            "channel inbound message",
            "inbound",
            None,
        );
        v.as_object_mut().unwrap().remove("id");
        Ok(v.to_string())
    });
    assert!(collect(lines, &[7; 32]).is_err());
}

#[test]
fn labeled_change_rates_keep_distinct_denominators() {
    let d = data(fixture());
    let first = &d.turns[0].pair_id;
    let second = &d.turns[1].pair_id;
    let pairs = json!({"schema_version":1,"experiment_id":d.experiment_id,"protocol_version":1,
    "pairs":[
        {"pair_id":first,"baseline":{"status":"ok","code":0},"treatment":{"status":"ok","code":1}},
        {"pair_id":second,"baseline":{"status":"ok","code":1},"treatment":{"status":"ok","code":1}}
    ]});
    let labels = json!({"schema_version":1,"experiment_id":d.experiment_id,"protocol_version":1,
        "rubric_version":1,"labels":[{"pair_id":first,"gold_code":1},{"pair_id":second,"gold_code":1}]});
    let r = report::report(
        &d,
        Some(serde_json::from_value(pairs.clone()).unwrap()),
        Some(serde_json::from_value(labels).unwrap()),
    )
    .unwrap();
    assert_eq!(r["decisions"]["label_coverage"], 1.0);
    assert_eq!(
        r["decisions"]["useful_change_rate_among_labeled_changes"],
        1.0
    );
    assert_eq!(
        r["decisions"]["useful_change_rate_among_labeled_pairs"],
        0.5
    );
    assert_eq!(
        r["decisions"]["harmful_change_rate_among_labeled_changes"],
        0.0
    );
    let unlabeled = report::report(&d, Some(serde_json::from_value(pairs).unwrap()), None).unwrap();
    assert_eq!(unlabeled["decisions"]["label_coverage"], 0.0);
    assert!(unlabeled["decisions"]["useful_change_rate_among_labeled_pairs"].is_null());
    assert!(unlabeled["decisions"]["useful_change_rate_among_labeled_changes"].is_null());
}

#[test]
fn files_reject_symlinks_permissions_hardlinks_and_line_overflow() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let path = root.join("source");
    private(&path, b"{}\n");
    let link = root.join("link");
    symlink(&path, &link).unwrap();
    assert!(files::open_private(&link).is_err());
    fs::hard_link(&path, root.join("hard")).unwrap();
    assert!(files::open_private(&path).is_err());
    fs::remove_file(root.join("hard")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(files::open_private(&path).is_err());
    private(&path, &vec![b'x'; zeroclaw_typesafe_eval::MAX_LINE + 1]);
    assert!(
        files::lines(files::open_private(&path).unwrap())
            .next()
            .unwrap()
            .is_err()
    );
}
