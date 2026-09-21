//! Weekly operations hygiene. Reconciles uncertain sends through the read-only
//! adapter path only (never a replay), triages dead-lettered events, and flags
//! reminders whose embedded deadline has passed. Reports one line, or nothing.
use crate::Ops;
use anyhow::Result;
use chrono::{Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

const REMIND_AGAIN_MS: i64 = 28 * 86_400_000;
const DIGEST_MAX_CHARS: usize = 600;

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS reminder_seen(id TEXT PRIMARY KEY,title TEXT NOT NULL,notes TEXT NOT NULL,due TEXT,first_seen_ms INTEGER NOT NULL,last_seen_ms INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS hygiene_reported(key TEXT PRIMARY KEY,reported_ms INTEGER NOT NULL);",
    )?;
    Ok(())
}

fn local_date(ms: i64) -> Option<NaiveDate> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.date_naive())
}

fn month_number(word: &str) -> Option<u32> {
    Some(match word {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

fn day_number(word: &str) -> Option<u32> {
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &word[digits.len()..];
    if !["", "st", "nd", "rd", "th"].contains(&suffix) || digits.is_empty() || digits.len() > 2 {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

/// Last calendar day covered by deadlines written into a reminder's text, or
/// `None` when it names none. `seen` is the local day the reminder was first
/// observed and anchors relative words ("today"). Several deadlines resolve to
/// the latest one, so a reminder only expires when every stated date is past.
pub fn embedded_deadline(text: &str, seen: NaiveDate) -> Option<NaiveDate> {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut latest: Option<NaiveDate> = None;
    let mut note = |d: NaiveDate| latest = Some(latest.map_or(d, |l| l.max(d)));
    for (i, word) in words.iter().enumerate() {
        match *word {
            "today" | "tonight" => note(seen),
            "tomorrow" => note(seen + Duration::days(1)),
            "this"
                if matches!(
                    words.get(i + 1),
                    Some(&("morning" | "afternoon" | "evening"))
                ) =>
            {
                note(seen)
            }
            _ => {}
        }
        if let Ok(date) = NaiveDate::parse_from_str(word, "%Y-%m-%d") {
            note(date);
        }
        if let (Some(month), Some(day)) = (
            month_number(word),
            words.get(i + 1).and_then(|w| day_number(w)),
        ) {
            // Nearest year to when the reminder was seen: a date months earlier
            // than that means next year's occurrence.
            let candidate = NaiveDate::from_ymd_opt(seen.year(), month, day).map(|d| {
                if d < seen - Duration::days(180) {
                    NaiveDate::from_ymd_opt(seen.year() + 1, month, day).unwrap_or(d)
                } else {
                    d
                }
            });
            if let Some(date) = candidate {
                note(date);
            }
        }
    }
    latest
}

fn clip(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max - 1).collect::<String>())
    }
}

fn dead_letter_next_step(error: &str) -> &'static str {
    let e = error.to_lowercase();
    if [
        "refresh token",
        "reauth",
        "unauthorized",
        "invalid_grant",
        "credential",
    ]
    .iter()
    .any(|k| e.contains(k))
    {
        "sign in to the account again"
    } else if ["timeout", "unavailable", "connection", "transport"]
        .iter()
        .any(|k| e.contains(k))
    {
        "no action unless it recurs"
    } else {
        "check connector health in the activity dashboard"
    }
}

impl Ops {
    /// Record every open reminder so relative deadlines have a stable anchor.
    pub fn reminders_seen(&self, reminders: &[Value]) -> Result<()> {
        let now = Utc::now().timestamp_millis();
        let tx = self.db.unchecked_transaction()?;
        for r in reminders {
            let Some(id) = r["id"].as_str() else { continue };
            self.db.execute(
                "INSERT INTO reminder_seen VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(id) DO UPDATE SET title=excluded.title,notes=excluded.notes,due=excluded.due,last_seen_ms=excluded.last_seen_ms",
                params![
                    id,
                    r["title"].as_str().unwrap_or(""),
                    clip(r["notes"].as_str().unwrap_or(""), 500),
                    r["due"].as_str(),
                    now
                ],
            )?;
        }
        // Completed or deleted reminders stop refreshing; forget them after a month.
        self.db.execute(
            "DELETE FROM reminder_seen WHERE last_seen_ms<?1",
            [now - 30 * 86_400_000],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn stale_reminders(&self, now_ms: i64) -> Result<Vec<Value>> {
        let Some(today) = local_date(now_ms) else {
            return Ok(vec![]);
        };
        // Only reminders the last refresh still listed as open.
        let rows = self
            .db
            .prepare(
                "SELECT id,title,notes,due,first_seen_ms FROM reminder_seen WHERE last_seen_ms>?1",
            )?
            .query_map([now_ms - 2 * 86_400_000], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut stale = vec![];
        for (id, title, notes, due, first_seen) in rows {
            let Some(seen) = local_date(first_seen) else {
                continue;
            };
            let Some(mut deadline) = embedded_deadline(&format!("{title}\n{notes}"), seen) else {
                continue;
            };
            // An explicit due date can only extend the deadline, never shorten it.
            if let Some(due) = due
                .and_then(|d| chrono::DateTime::parse_from_rfc3339(&d).ok())
                .and_then(|d| local_date(d.timestamp_millis()))
            {
                deadline = deadline.max(due);
            }
            if deadline < today {
                stale.push(json!({"id":id,"title":title,"deadline":deadline.to_string()}));
            }
        }
        stale.sort_by_key(|s| s["id"].as_str().map(str::to_owned));
        Ok(stale)
    }

    fn dead_letters(&self) -> Result<Vec<Value>> {
        Ok(self
            .db
            .prepare("SELECT id,source,kind,attempts,COALESCE(last_error,'') FROM event_inbox WHERE state='dead_letter' ORDER BY created_ms")?
            .query_map([], |r| {
                let error: String = r.get(4)?;
                Ok(json!({"id":r.get::<_,String>(0)?,"source":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"attempts":r.get::<_,i64>(3)?,"next_step":dead_letter_next_step(&error),"cause":clip(&error,300)}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn uncertain_operation_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare("SELECT DISTINCT s.operation_id FROM operation_steps s JOIN operations o ON o.id=s.operation_id WHERE s.state='uncertain' AND o.cancelled=0 AND o.authorized_ms IS NOT NULL ORDER BY s.operation_id")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Hygiene pass. Reconciliation is read-only and never sends; a failed check
    /// leaves the operation uncertain. Nothing is notified here.
    pub async fn weekly_hygiene(&self) -> Result<Value> {
        let before = self.uncertain_operation_ids()?;
        let mut still_uncertain = vec![];
        for id in &before {
            let state = match self.reconcile_operation(id).await {
                Ok(status) => status["state"].as_str().unwrap_or("uncertain").to_owned(),
                Err(_) => "uncertain".to_owned(),
            };
            if state == "uncertain" {
                let title = self.operation_status(id).ok().and_then(|s| {
                    s["review"]["title"]
                        .as_str()
                        .or(s["review"]["subject"].as_str())
                        .map(|t| clip(t, 60))
                });
                still_uncertain.push(json!({"id":id,"title":title}));
            }
        }
        Ok(json!({
            "reconciled": before.len() - still_uncertain.len(),
            "uncertain": still_uncertain,
            "dead_letters": self.dead_letters()?,
            "stale_reminders": self.stale_reminders(Utc::now().timestamp_millis())?,
        }))
    }

    fn unreported(&self, key: &str, now_ms: i64) -> Result<bool> {
        Ok(self
            .db
            .query_row(
                "SELECT reported_ms FROM hygiene_reported WHERE key=?1",
                [key],
                |r| r.get::<_, i64>(0),
            )
            .map(|at| now_ms - at >= REMIND_AGAIN_MS)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(true),
                e => Err(e),
            })?)
    }

    /// Build the digest for items not mentioned in the last four weeks. Returns
    /// `None` when there is nothing new, plus the keys the digest covers.
    pub fn hygiene_digest(
        &self,
        report: &Value,
        now_ms: i64,
    ) -> Result<Option<(String, Vec<String>)>> {
        let mut keys = vec![];
        let mut parts = vec![];
        let fresh = |kind: &str, id: &str, keys: &mut Vec<String>| -> Result<bool> {
            let key = format!("{kind}:{id}");
            let new = self.unreported(&key, now_ms)?;
            if new {
                keys.push(key);
            }
            Ok(new)
        };
        let mut uncertain = vec![];
        for item in report["uncertain"].as_array().into_iter().flatten() {
            if fresh("uncertain", item["id"].as_str().unwrap_or(""), &mut keys)? {
                uncertain.push(
                    item["title"]
                        .as_str()
                        .unwrap_or("untitled action")
                        .to_owned(),
                );
            }
        }
        if !uncertain.is_empty() {
            parts.push(format!(
                "{} send{} still unverified after a read-only recheck, do not resend without checking ({})",
                uncertain.len(),
                if uncertain.len() == 1 { "" } else { "s" },
                {
                    let shown = uncertain.iter().take(3).cloned().collect::<Vec<_>>();
                    match uncertain.len() - shown.len() {
                        0 => shown.join("; "),
                        more => format!("{}; +{more} more", shown.join("; ")),
                    }
                }
            ));
        }
        let mut dead: Vec<(String, String)> = vec![];
        for item in report["dead_letters"].as_array().into_iter().flatten() {
            if fresh("dead_letter", item["id"].as_str().unwrap_or(""), &mut keys)? {
                dead.push((
                    format!(
                        "{} {}",
                        item["source"].as_str().unwrap_or("?"),
                        item["kind"].as_str().unwrap_or("event")
                    ),
                    item["next_step"].as_str().unwrap_or("").to_owned(),
                ));
            }
        }
        if !dead.is_empty() {
            let mut steps: Vec<&str> = dead.iter().map(|d| d.1.as_str()).collect();
            steps.sort_unstable();
            steps.dedup();
            parts.push(format!(
                "{} dead-lettered event{} ({}): {}",
                dead.len(),
                if dead.len() == 1 { "" } else { "s" },
                dead.iter()
                    .map(|d| d.0.as_str())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(", "),
                steps.join(" / ")
            ));
        }
        let mut stale = vec![];
        for item in report["stale_reminders"].as_array().into_iter().flatten() {
            let id = format!(
                "{}:{}",
                item["id"].as_str().unwrap_or(""),
                item["deadline"].as_str().unwrap_or("")
            );
            if fresh("reminder", &id, &mut keys)? {
                stale.push(format!(
                    "\"{}\"",
                    clip(item["title"].as_str().unwrap_or(""), 50)
                ));
            }
        }
        if !stale.is_empty() {
            parts.push(format!(
                "{} reminder{} past an embedded deadline ({})",
                stale.len(),
                if stale.len() == 1 { "" } else { "s" },
                stale.join(", ")
            ));
        }
        if parts.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            clip(
                &format!("Weekly ops check: {}.", parts.join("; ")),
                DIGEST_MAX_CHARS,
            ),
            keys,
        )))
    }

    /// Run the pass and, unless `dry_run`, send the digest and remember what it
    /// covered so unresolved items are not repeated for four weeks.
    pub async fn run_weekly_hygiene(&self, dry_run: bool) -> Result<Value> {
        let report = self.weekly_hygiene().await?;
        let now = Utc::now();
        let Some((digest, keys)) = self.hygiene_digest(&report, now.timestamp_millis())? else {
            return Ok(json!({"digest":null,"sent":false,"report":report}));
        };
        if dry_run {
            return Ok(json!({"digest":digest,"sent":false,"dry_run":true,"report":report}));
        }
        let week = now.iso_week();
        let mut sorted = keys.clone();
        sorted.sort();
        let alert_key = format!(
            "hygiene:{}-W{:02}:{}",
            week.year(),
            week.week(),
            &crate::digest(sorted.join("|").as_bytes())[..12]
        );
        let sent = self.alert_owner(&alert_key, &digest).await?;
        for key in &keys {
            self.db.execute(
                "INSERT INTO hygiene_reported VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET reported_ms=excluded.reported_ms",
                params![key, now.timestamp_millis()],
            )?;
        }
        Ok(json!({"digest":digest,"sent":sent,"report":report}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn relative_words_anchor_to_first_seen_day() {
        let seen = day(2026, 9, 17);
        for text in [
            "Konrad passport: decide whether to reschedule (free window ends 2:40 PM today)",
            "Call the dentist TONIGHT",
            "check the oven this afternoon",
        ] {
            assert_eq!(embedded_deadline(text, seen), Some(seen), "{text}");
        }
        assert_eq!(
            embedded_deadline("Send forms tomorrow", seen),
            Some(day(2026, 9, 18))
        );
    }

    #[test]
    fn explicit_dates_and_latest_wins() {
        let seen = day(2026, 9, 17);
        assert_eq!(
            embedded_deadline("File by 2026-09-30", seen),
            Some(day(2026, 9, 30))
        );
        assert_eq!(
            embedded_deadline("Renew by Sept 25th, ideally today", seen),
            Some(day(2026, 9, 25))
        );
        assert_eq!(
            embedded_deadline("Register before Jan 5", seen),
            Some(day(2027, 1, 5)),
            "a month long past means the next occurrence"
        );
    }

    #[test]
    fn ordinary_text_has_no_deadline() {
        let seen = day(2026, 9, 17);
        for text in [
            "Buy milk",
            "Read chapter 5 of the book",
            "may need to call back",
            "Compare 3/4 inch bolts",
            "",
        ] {
            assert_eq!(embedded_deadline(text, seen), None, "{text}");
        }
    }

    fn ops() -> Result<(tempfile::TempDir, Ops)> {
        let dir = tempfile::tempdir()?;
        let ops = Ops::open(dir.path())?;
        Ok((dir, ops))
    }

    fn age_reminder(ops: &Ops, id: &str, days: i64) -> Result<()> {
        let at = Utc::now().timestamp_millis() - days * 86_400_000;
        ops.db.execute(
            "UPDATE reminder_seen SET first_seen_ms=?2 WHERE id=?1",
            params![id, at],
        )?;
        Ok(())
    }

    #[test]
    fn only_expired_deadlines_are_stale() -> Result<()> {
        let (_dir, ops) = ops()?;
        ops.reminders_seen(&[
            json!({"id":"old","title":"Decide by 2:40 PM today","notes":"","due":null}),
            json!({"id":"fresh","title":"Decide by 2:40 PM today","notes":"","due":null}),
            json!({"id":"plain","title":"Buy milk","notes":"","due":null}),
            json!({"id":"extended","title":"Pay today","notes":"","due":(Utc::now()+Duration::days(3)).to_rfc3339()}),
        ])?;
        for id in ["old", "extended", "plain"] {
            age_reminder(&ops, id, 3)?;
        }
        let stale = ops.stale_reminders(Utc::now().timestamp_millis())?;
        assert_eq!(stale.len(), 1, "{stale:?}");
        assert_eq!(stale[0]["id"], "old");
        Ok(())
    }

    #[test]
    fn reminders_no_longer_listed_are_ignored() -> Result<()> {
        let (_dir, ops) = ops()?;
        ops.reminders_seen(&[json!({"id":"gone","title":"today only","notes":"","due":null})])?;
        age_reminder(&ops, "gone", 5)?;
        ops.db.execute(
            "UPDATE reminder_seen SET last_seen_ms=?1",
            [Utc::now().timestamp_millis() - 3 * 86_400_000],
        )?;
        assert!(
            ops.stale_reminders(Utc::now().timestamp_millis())?
                .is_empty()
        );
        Ok(())
    }

    fn dead_letter(ops: &Ops, id: &str, error: &str) -> Result<()> {
        ops.db.execute(
            "INSERT INTO event_inbox(id,source,kind,payload,state,attempts,next_ms,created_ms,last_error) VALUES(?1,'gmail','email_changed','{}','dead_letter',5,0,1,?2)",
            params![id, error],
        )?;
        Ok(())
    }

    #[test]
    fn digest_lists_at_most_three_uncertain_sends() -> Result<()> {
        let (_dir, ops) = ops()?;
        let uncertain: Vec<Value> = (0..6)
            .map(|i| json!({"id":format!("op{i}"),"title":format!("Send {i}")}))
            .collect();
        let report = json!({"uncertain":uncertain,"dead_letters":[],"stale_reminders":[]});
        let (digest, keys) = ops.hygiene_digest(&report, 0)?.expect("uncertain sends");
        assert!(digest.contains("6 sends still unverified"), "{digest}");
        assert!(
            digest.contains("Send 0; Send 1; Send 2; +3 more"),
            "{digest}"
        );
        assert!(!digest.contains("Send 3"));
        assert_eq!(
            keys.len(),
            6,
            "every send is marked reported, not only the ones shown"
        );
        Ok(())
    }

    #[tokio::test]
    async fn clean_state_is_silent() -> Result<()> {
        let (_dir, ops) = ops()?;
        let result = ops.run_weekly_hygiene(true).await?;
        assert!(result["digest"].is_null());
        assert_eq!(result["report"]["uncertain"], json!([]));
        Ok(())
    }

    #[tokio::test]
    async fn dead_letters_report_cause_and_next_step_once_per_month() -> Result<()> {
        let (_dir, ops) = ops()?;
        dead_letter(
            &ops,
            "a",
            "Google connector failed: refresh token expired or revoked",
        )?;
        dead_letter(
            &ops,
            "b",
            "Google connector failed: refresh token expired or revoked",
        )?;
        let report = ops.weekly_hygiene().await?;
        assert_eq!(
            report["dead_letters"][0]["next_step"],
            "sign in to the account again"
        );
        let now = Utc::now().timestamp_millis();
        let (digest, keys) = ops.hygiene_digest(&report, now)?.expect("new dead letters");
        assert!(
            digest.starts_with("Weekly ops check: 2 dead-lettered events (gmail email_changed)"),
            "{digest}"
        );
        assert!(digest.contains("sign in to the account again"));
        assert!(!digest.contains('\n'));
        for key in &keys {
            ops.db.execute(
                "INSERT INTO hygiene_reported VALUES(?1,?2)",
                params![key, now],
            )?;
        }
        assert!(
            ops.hygiene_digest(&report, now + 7 * 86_400_000)?.is_none(),
            "already reported"
        );
        assert!(
            ops.hygiene_digest(&report, now + 29 * 86_400_000)?
                .is_some(),
            "re-mentioned after four weeks"
        );
        Ok(())
    }

    #[tokio::test]
    async fn uncertain_sends_are_rechecked_read_only_and_never_replayed() -> Result<()> {
        let (_dir, ops) = ops()?;
        let review = ops.outbox_prepare(
            &json!({"idempotency_key":"hygiene-fixture","channel":"email",
            "recipients":["recipient@example.invalid"],"subject":"Fixture subject","text":"Body"}),
        )?;
        ops.operation_authorize(
            &json!({"operation_id":review["operation_id"],"review_hash":review["review_hash"],
            "review":review["review"],"owner_requested_send":true}),
        )?;
        // A crash after the write-ahead claim leaves the step uncertain.
        ops.db.execute(
            "UPDATE operation_steps SET state='uncertain' WHERE operation_id='hygiene-fixture'",
            [],
        )?;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let status = ops
            .operation_reconcile_using("hygiene-fixture", |_, _, reconcile| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    assert!(reconcile, "hygiene must only use the read-only path");
                    Ok(Outcome::uncertain("no exact receipt"))
                }
            })
            .await?;
        assert_eq!(status["state"], "uncertain");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let resolved = ops
            .operation_reconcile_using("hygiene-fixture", |_, _, _| async {
                Ok(Outcome {
                    state: "submitted".into(),
                    evidence: json!({"verification":"found_in_sent"}),
                })
            })
            .await?;
        assert_eq!(resolved["state"], "submitted");
        Ok(())
    }

    #[tokio::test]
    async fn reconcile_only_never_starts_prepared_steps() -> Result<()> {
        let (_dir, ops) = ops()?;
        let review = ops.operation_prepare(&json!({"idempotency_key":"two-step","steps":[
            {"tool":"outbox_email","arguments":{"recipients":["a@example.invalid"],"subject":"One","text":"1","attachments":[]}},
            {"tool":"outbox_email","arguments":{"recipients":["b@example.invalid"],"subject":"Two","text":"2","attachments":[]}}]}))?;
        ops.operation_authorize(
            &json!({"operation_id":review["operation_id"],"review_hash":review["review_hash"],
            "review":review["review"],"owner_requested_send":true}),
        )?;
        ops.db.execute(
            "UPDATE operation_steps SET state='uncertain' WHERE operation_id='two-step' AND ordinal=0",
            [],
        )?;
        let status = ops
            .operation_reconcile_using("two-step", |_, _, reconcile| async move {
                assert!(
                    reconcile,
                    "only the uncertain step may run, and only read-only"
                );
                Ok(Outcome {
                    state: "submitted".into(),
                    evidence: json!({}),
                })
            })
            .await?;
        assert_eq!(status["steps"][0]["state"], "submitted");
        assert_eq!(
            status["steps"][1]["state"], "prepared",
            "second step untouched"
        );
        Ok(())
    }

    use crate::journal::Outcome;
}
