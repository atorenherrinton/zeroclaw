//! Side-effect-free reading plans. User-confirmed facts and the scheduler's
//! delivery receipts remain canonical; this tool stores nothing and sends nothing.
use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};

const MAX_PAGES: u32 = 10_000;
const MAX_DAYS_AHEAD: i64 = 366;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadingInput {
    remaining_pages: Option<u32>,
    due_date: Option<String>,
    timezone: Option<String>,
    #[serde(default)]
    reminder: Option<ReminderInput>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReminderInput {
    // Explicit opt-in. An omitted reminder block never recommends a check-in.
    enabled: bool,
    paused: bool,
    local_hour: u32,
    // Successful delivery receipt, not a generated draft or attempted send.
    last_delivered_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct ReadingCoachTool;

#[async_trait]
impl Tool for ReadingCoachTool {
    fn name(&self) -> &str {
        "reading_coach"
    }

    fn description(&self) -> &str {
        "Plan reading from confirmed remaining pages, a YYYY-MM-DD due date (inclusive local end of day), and an IANA time zone. Recalculate after each progress update. Returns missing-input questions and optional gentle check-in eligibility. Never stores facts, schedules, or sends messages."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "remaining_pages": {"type": "integer", "minimum": 0, "maximum": MAX_PAGES},
                "due_date": {"type": "string", "description": "Confirmed YYYY-MM-DD; inclusive local end of day. For a morning deadline confirm the previous reading day."},
                "timezone": {"type": "string", "description": "Confirmed IANA zone, e.g. Europe/Berlin; never infer from the server."},
                "reminder": {
                    "type": "object", "additionalProperties": false,
                    "required": ["enabled", "paused", "local_hour"],
                    "properties": {
                        "enabled": {"type": "boolean"},
                        "paused": {"type": "boolean"},
                        "local_hour": {"type": "integer", "minimum": 8, "maximum": 19},
                        "last_delivered_at": {"type": "string", "format": "date-time", "description": "Actual successful check-in delivery receipt with UTC offset; omit only before the first delivery. Never retry an uncertain send."}
                    }
                }
            }
        })
    }

    async fn execute(&self, args: Value) -> anyhow::Result<ToolResult> {
        match evaluate(args, Utc::now()) {
            Ok(value) => Ok(ToolResult {
                success: true,
                output: ToolOutput::json(value),
                error: None,
            }),
            Err(error) => Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            }),
        }
    }
}

fn evaluate(args: Value, now: DateTime<Utc>) -> Result<Value, String> {
    let input: ReadingInput = serde_json::from_value(args)
        .map_err(|_| "Invalid reading input: use only documented fields and types.".to_string())?;
    if input.remaining_pages.is_some_and(|pages| pages > MAX_PAGES) {
        return Err(format!(
            "remaining_pages must be between 0 and {MAX_PAGES}."
        ));
    }
    let due = input.due_date.as_deref().map(parse_date).transpose()?;
    let zone = input
        .timezone
        .as_deref()
        .map(|value| {
            value
                .parse::<Tz>()
                .map_err(|_| "timezone must be a valid IANA time zone.".to_string())
        })
        .transpose()?;
    if let Some(reminder) = &input.reminder {
        if !(8..=19).contains(&reminder.local_hour) {
            return Err("local_hour must be between 8 and 19 (daytime check-ins only).".into());
        }
        if reminder.last_delivered_at.is_some_and(|last| last > now) {
            return Err("last_delivered_at cannot be in the future.".into());
        }
    }

    let mut questions = Vec::new();
    if input.remaining_pages.is_none() {
        questions.push("How many pages do you have left to read?");
    }
    if due.is_none() {
        questions.push(
            "What date do you need to finish by? Is that before school or by the end of the day?",
        );
    }
    if zone.is_none() {
        questions.push("What time zone should we use? A parent or trusted adult can help; no address is needed.");
    }
    let (Some(pages), Some(due), Some(zone)) = (input.remaining_pages, due, zone) else {
        return Ok(json!({
            "status": "needs_input", "questions": questions,
            "check_in": {"eligible": false, "reason": "needs_input"}
        }));
    };
    let local_now = now.with_timezone(&zone);
    let today = local_now.date_naive();
    let days_ahead = (due - today).num_days();
    if days_ahead > MAX_DAYS_AHEAD {
        return Err(format!(
            "due_date must be no more than {MAX_DAYS_AHEAD} days ahead."
        ));
    }
    // Inclusive local calendar days, not 24-hour durations: DST days count once.
    let days = (days_ahead + 1).max(0) as u32;
    let status = if pages == 0 {
        "completed"
    } else if days == 0 {
        "overdue"
    } else {
        "active"
    };
    let daily_target = if pages == 0 {
        Some(0)
    } else if days > 0 {
        Some(pages.div_ceil(days))
    } else {
        None
    };
    let average = if pages == 0 {
        Some(0.0)
    } else if days > 0 {
        Some(f64::from(pages) / f64::from(days))
    } else {
        None
    };
    let reason = match &input.reminder {
        _ if status == "completed" => "completed",
        _ if status == "overdue" => "overdue_replan_with_adult",
        None => "not_configured",
        Some(reminder) if !reminder.enabled => "disabled",
        Some(reminder) if reminder.paused => "paused",
        Some(reminder)
            if reminder.last_delivered_at.is_some_and(|last| {
                // Guard both local calendar duplicates and zone changes/DST.
                last.with_timezone(&zone).date_naive() >= today || (now - last).num_hours() < 20
            }) =>
        {
            "already_delivered"
        }
        Some(reminder) if local_now.hour() != reminder.local_hour => "outside_check_in_hour",
        Some(_) => "ready",
    };
    let message = match status {
        "completed" => "You finished! Nice work. Reading reminders can stop now.".to_string(),
        "overdue" => "The reading date has passed. We can make a new plan with a parent or teacher; no need to rush or feel bad.".to_string(),
        _ => format!(
            "You have {pages} pages left and {days} reading days, including today. Aim for about {} pages a day, stopping when you finish. How many pages are left now? It's okay to ask for a smaller plan or pause check-ins.",
            daily_target.unwrap_or(0)
        ),
    };
    Ok(json!({
        "status": status,
        "as_of_local_date": today.to_string(),
        "timezone": zone.name(),
        "due_date": due.to_string(),
        "deadline_convention": "inclusive_local_end_of_day",
        "remaining_pages": pages,
        "reading_days": days,
        "required_average_pages_per_day": average,
        "suggested_daily_pages": daily_target,
        "message": message,
        "check_in": {"eligible": reason == "ready", "reason": reason},
        "delivery": "recommendation_only_no_message_sent"
    }))
}

fn parse_date(value: &str) -> Result<NaiveDate, String> {
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map_err(|_| "due_date must be a real YYYY-MM-DD date.".to_string())?;
    if value.len() != 10 || date.year() < 1 || date.format("%Y-%m-%d").to_string() != value {
        return Err("due_date must use exactly YYYY-MM-DD.".into());
    }
    Ok(date)
}

#[cfg(test)]
mod tests;
