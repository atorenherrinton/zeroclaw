use super::*;
use std::path::Path;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::Config;

fn at(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}

fn input() -> Value {
    json!({"remaining_pages": 101, "due_date": "2026-09-20", "timezone": "Europe/Berlin"})
}

fn plan(args: Value, now: &str) -> Value {
    evaluate(args, at(now)).unwrap()
}

fn with_reminder() -> Value {
    let mut args = input();
    args["reminder"] = json!({"enabled": true, "paused": false, "local_hour": 17});
    args
}

#[test]
fn inclusive_days_ceil_and_updated_required_average() {
    let result = plan(input(), "2026-09-17T12:00:00Z");
    assert_eq!(result["reading_days"], 4);
    assert_eq!(result["suggested_daily_pages"], 26);
    assert_eq!(result["required_average_pages_per_day"], 25.25);
    let mut next = input();
    next["remaining_pages"] = json!(61);
    let result = plan(next, "2026-09-18T12:00:00Z");
    assert_eq!(result["reading_days"], 3);
    assert_eq!(result["suggested_daily_pages"], 21);
    assert_eq!(result["required_average_pages_per_day"], json!(61.0 / 3.0));
}

#[test]
fn integer_targets_always_cover_remaining_pages_without_overflow() {
    for pages in [0_u32, 1, 3, 101, MAX_PAGES] {
        for offset in [0_i64, 1, 10, MAX_DAYS_AHEAD] {
            let now = at("2026-09-17T12:00:00Z");
            let date = now.date_naive() + chrono::Duration::days(offset);
            let result = evaluate(
                json!({
                    "remaining_pages": pages, "due_date": date.to_string(), "timezone": "UTC"
                }),
                now,
            )
            .unwrap();
            let target = result["suggested_daily_pages"].as_u64().unwrap();
            let days = result["reading_days"].as_u64().unwrap();
            assert!(target * days >= u64::from(pages));
            assert!(target == 0 || (target - 1) * days < u64::from(pages));
        }
    }
}

#[test]
fn due_today_overdue_and_complete_never_divide_by_zero() {
    let today = plan(input(), "2026-09-20T12:00:00Z");
    assert_eq!(today["reading_days"], 1);
    assert_eq!(today["suggested_daily_pages"], 101);
    let overdue = plan(with_reminder(), "2026-09-21T15:00:00Z");
    assert_eq!(overdue["status"], "overdue");
    assert!(overdue["required_average_pages_per_day"].is_null());
    assert_eq!(overdue["check_in"]["eligible"], false);
    let mut args = with_reminder();
    args["remaining_pages"] = json!(0);
    let done = plan(args, "2026-09-21T15:00:00Z");
    assert_eq!(done["status"], "completed");
    assert_eq!(done["suggested_daily_pages"], 0);
    assert_eq!(done["check_in"]["reason"], "completed");
}

#[test]
fn zones_not_server_date_determine_reading_days() {
    let mut args = input();
    args["timezone"] = json!("America/Los_Angeles");
    let west = plan(args.clone(), "2026-09-18T01:00:00Z");
    args["timezone"] = json!("Asia/Tokyo");
    let east = plan(args, "2026-09-18T01:00:00Z");
    assert_eq!(west["reading_days"], 4);
    assert_eq!(east["reading_days"], 3);
}

#[test]
fn leap_dates_and_year_rollover() {
    for (now, due) in [
        ("2028-02-28T12:00:00Z", "2028-03-01"),
        ("2026-12-30T12:00:00Z", "2027-01-01"),
    ] {
        let mut args = input();
        args["due_date"] = json!(due);
        assert_eq!(plan(args, now)["reading_days"], 3);
    }
}

#[test]
fn dst_uses_calendar_days_and_local_check_in_hour() {
    for (date, before, after) in [
        ("2026-03-09", "2026-03-07T22:00:00Z", "2026-03-08T21:00:00Z"),
        ("2026-11-02", "2026-10-31T21:00:00Z", "2026-11-01T22:00:00Z"),
    ] {
        let mut args = with_reminder();
        args["timezone"] = json!("America/New_York");
        args["due_date"] = json!(date);
        assert_eq!(plan(args.clone(), before)["reading_days"], 3);
        args["reminder"]["last_delivered_at"] = json!(before);
        let result = plan(args, after);
        assert_eq!(result["reading_days"], 2);
        assert_eq!(result["check_in"]["eligible"], true);
    }
}

#[test]
fn missing_facts_ask_without_inventing_a_plan_or_reminder() {
    let result = plan(json!({}), "2026-09-17T15:00:00Z");
    assert_eq!(result["status"], "needs_input");
    assert_eq!(result["questions"].as_array().unwrap().len(), 3);
    assert!(result.get("suggested_daily_pages").is_none());
    assert_eq!(result["check_in"]["eligible"], false);
    for key in ["remaining_pages", "due_date", "timezone"] {
        let mut args = with_reminder();
        args.as_object_mut().unwrap().remove(key);
        assert_eq!(
            plan(args, "2026-09-17T15:00:00Z")["check_in"]["eligible"],
            false
        );
    }
}

#[test]
fn reject_bad_pages_dates_zones_and_unknown_fields() {
    let now = at("2026-09-17T15:00:00Z");
    for bad in [
        json!(-1),
        json!(1.5),
        json!(10_001),
        json!(u64::MAX),
        json!("100"),
        json!(true),
    ] {
        let mut args = input();
        args["remaining_pages"] = bad;
        assert!(evaluate(args, now).is_err());
    }
    for bad in [
        "2026-02-29",
        "2026-9-20",
        "tomorrow",
        "2026-09-20T00:00:00Z",
        "2027-09-19",
        "0000-01-01",
    ] {
        let mut args = input();
        args["due_date"] = json!(bad);
        assert!(evaluate(args, now).is_err(), "accepted {bad}");
    }
    for bad in ["", "Mars/Olympus", "+02:00"] {
        let mut args = input();
        args["timezone"] = json!(bad);
        assert!(evaluate(args, now).is_err());
    }
    let mut args = input();
    args["destination"] = json!("untrusted-recipient");
    assert!(evaluate(args, now).is_err());
    assert!(evaluate(json!([]), now).is_err());
}

#[test]
fn reminders_are_opt_in_daytime_paused_and_once_per_day() {
    assert_eq!(
        plan(input(), "2026-09-17T15:00:00Z")["check_in"]["reason"],
        "not_configured"
    );
    let args = with_reminder();
    assert_eq!(
        plan(args.clone(), "2026-09-17T15:00:00Z")["check_in"]["eligible"],
        true
    );
    for time in [
        "2026-09-17T14:59:59Z",
        "2026-09-17T16:00:00Z",
        "2026-09-17T23:00:00Z",
    ] {
        assert_eq!(plan(args.clone(), time)["check_in"]["eligible"], false);
    }
    for (key, value, reason) in [("paused", true, "paused"), ("enabled", false, "disabled")] {
        let mut changed = args.clone();
        changed["reminder"][key] = json!(value);
        assert_eq!(
            plan(changed, "2026-09-17T15:00:00Z")["check_in"]["reason"],
            reason
        );
    }
    for last in [
        "2026-09-17T15:00:00Z",
        "2026-09-17T06:00:00Z",
        "2026-09-16T20:00:00Z",
    ] {
        let mut changed = args.clone();
        changed["reminder"]["last_delivered_at"] = json!(last);
        assert_eq!(
            plan(changed, "2026-09-17T15:00:00Z")["check_in"]["reason"],
            "already_delivered"
        );
    }
}

#[test]
fn reminder_validation_fails_closed() {
    for hour in [0, 7, 20, 24] {
        let mut args = with_reminder();
        args["reminder"]["local_hour"] = json!(hour);
        assert!(evaluate(args, at("2026-09-17T15:00:00Z")).is_err());
    }
    for last in ["2026-09-18T00:00:00Z", "2026-09-16", "2026-09-16T12:00:00"] {
        let mut args = with_reminder();
        args["reminder"]["last_delivered_at"] = json!(last);
        assert!(evaluate(args, at("2026-09-17T15:00:00Z")).is_err());
    }
    for key in ["enabled", "paused", "local_hour"] {
        let mut args = with_reminder();
        args["reminder"].as_object_mut().unwrap().remove(key);
        assert!(evaluate(args, at("2026-09-17T15:00:00Z")).is_err());
    }
}

#[tokio::test]
async fn public_tool_boundary_is_structured_and_non_sending() {
    let tool = ReadingCoachTool;
    assert_eq!(tool.name(), "reading_coach");
    assert_eq!(tool.parameters_schema()["additionalProperties"], false);
    let missing = tool.execute(json!({})).await.unwrap();
    assert!(missing.success);
    assert_eq!(missing.output.data().unwrap()["status"], "needs_input");
    let rejected = tool.execute(json!({"remaining_pages": -1})).await.unwrap();
    assert!(!rejected.success);
    assert!(rejected.error.is_some());
}

#[test]
fn shipped_agent_profile_is_closed_and_tool_policy_enforced() {
    let config: Config = toml::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/agents/reading-coach/config.fragment.toml"
    )))
    .unwrap();
    let agent = &config.agents["reading_coach"];
    assert!(!agent.enabled);
    assert!(agent.channels.is_empty());
    assert!(agent.mcp_bundles.is_empty());
    assert!(agent.skill_bundles.is_empty());
    assert!(agent.knowledge_bundles.is_empty());
    assert!(agent.cron_jobs.is_empty());
    assert!(agent.delegates.is_empty());
    assert!(!agent.delegate_same_risk_profile);
    assert!(!agent.acp_enable_mcp);
    let risk = &config.risk_profiles["reading_coach"];
    let runtime = &config.runtime_profiles["reading_coach"];
    assert!(risk.allowed_commands.is_empty());
    assert!(risk.allowed_roots.is_empty());
    assert!(risk.shell_env_passthrough.is_empty());
    assert_eq!(risk.auto_approve, ["reading_coach"]);
    assert_eq!(runtime.max_tool_iterations, 3);
    assert_eq!(runtime.max_delegation_depth, 0);
    assert_eq!(
        risk.delegation_policy.mode,
        zeroclaw_config::autonomy::DelegationMode::Forbidden
    );
    let policy =
        SecurityPolicy::from_profiles(risk, Some(runtime), Path::new("/reading-coach-test"));
    assert!(policy.is_tool_allowed("reading_coach"));
    for name in [
        "shell",
        "file_read",
        "file_write",
        "memory_recall",
        "memory_store",
        "browser",
        "web_fetch",
        "delegate",
        "spawn_subagent",
        "cron_add",
        "schedule",
        "send_via",
        "mcp__send",
        "unknown_future_tool",
    ] {
        assert!(!policy.is_tool_allowed(name), "unexpected grant: {name}");
    }
}
