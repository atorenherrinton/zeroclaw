//! Durable deduplicated event inbox. Push/file events wake refresh immediately;
//! bounded periodic reconciliation covers providers without an installed watch.
use crate::{Ops, outbox, text};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS event_inbox(id TEXT PRIMARY KEY,source TEXT NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL,state TEXT NOT NULL DEFAULT 'pending',attempts INTEGER NOT NULL DEFAULT 0,next_ms INTEGER NOT NULL,created_ms INTEGER NOT NULL,last_error TEXT);
 CREATE INDEX IF NOT EXISTS event_due ON event_inbox(state,next_ms);
 CREATE TABLE IF NOT EXISTS connector_health(name TEXT PRIMARY KEY,state TEXT NOT NULL,consecutive_failures INTEGER NOT NULL,last_checked_ms INTEGER NOT NULL,last_success_ms INTEGER,detail TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS alert_receipts(key TEXT PRIMARY KEY,state TEXT NOT NULL,created_ms INTEGER NOT NULL);")?;
    Ok(())
}
impl Ops {
    pub fn event_ingest(&self, a: &Value) -> Result<Value> {
        let id = text(a, "event_id", 256)?;
        let source = text(a, "source", 64)?;
        let kind = text(a, "kind", 64)?;
        ensure!(
            [
                "gmail",
                "calendar",
                "reminders",
                "github",
                "runtime",
                "shipments",
                "local"
            ]
            .contains(&source),
            "unrecognized event source"
        );
        ensure!(
            [
                "email_changed",
                "calendar_changed",
                "invitation_response",
                "reminder_overdue",
                "github_changed",
                "automation_failed",
                "shipment_changed",
                "reconcile"
            ]
            .contains(&kind),
            "unrecognized event kind"
        );
        let payload = a.get("payload").cloned().unwrap_or(json!({}));
        ensure!(
            payload.to_string().len() <= 64000,
            "event payload too large"
        );
        let key = crate::digest(format!("{source}:{id}").as_bytes());
        let now = Utc::now().timestamp_millis();
        let inserted=self.db.execute("INSERT OR IGNORE INTO event_inbox(id,source,kind,payload,next_ms,created_ms) VALUES(?1,?2,?3,?4,?5,?5)",params![key,source,kind,payload.to_string(),now])?;
        Ok(json!({"accepted":true,"duplicate_prevented":inserted==0,"event_key":key}))
    }
    pub fn health_record(&self, name: &str, state: &str, detail: &str) -> Result<()> {
        ensure!(
            [
                "healthy",
                "temporary_outage",
                "unsupported",
                "not_configured",
                "authentication_required"
            ]
            .contains(&state),
            "invalid connector health state"
        );
        let now = Utc::now().timestamp_millis();
        self.db.execute("INSERT INTO connector_health VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(name) DO UPDATE SET state=excluded.state,consecutive_failures=CASE WHEN excluded.state='healthy' THEN 0 ELSE connector_health.consecutive_failures+1 END,last_checked_ms=excluded.last_checked_ms,last_success_ms=COALESCE(excluded.last_success_ms,connector_health.last_success_ms),detail=excluded.detail",params![name,state,if state=="healthy"{0}else{1},now,if state=="healthy"{Some(now)}else{None},detail])?;
        Ok(())
    }
    pub fn activity(&self) -> Result<Value> {
        let ids = self
            .db
            .prepare("SELECT id FROM operations ORDER BY created_ms DESC LIMIT 100")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let actions = ids
            .iter()
            .map(|id| self.operation_status(id))
            .collect::<Result<Vec<_>>>()?;
        let receipts=self.db.prepare("SELECT sequence,operation_id,ordinal,state,evidence,created_ms FROM operation_receipts ORDER BY sequence DESC LIMIT 150")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<usize>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(sequence,id,ordinal,state,evidence,at)|Ok(json!({"sequence":sequence,"id":id,"ordinal":ordinal,"state":state,"evidence":serde_json::from_str::<Value>(&evidence)?,"created_ms":at}))).collect::<Result<Vec<_>>>()?;
        let sources=self.db.prepare("SELECT source,payload,verified_ms,error FROM source_snapshots ORDER BY source")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Option<String>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().map(|(source,payload,at,error)|Ok(json!({"source":source,"data":serde_json::from_str::<Value>(&payload)?,"verified_ms":at,"stale":Utc::now().timestamp_millis()-at>30*60_000,"error":error}))).collect::<Result<Vec<_>>>()?;
        let health=self.db.prepare("SELECT name,state,consecutive_failures,last_checked_ms,last_success_ms,detail FROM connector_health ORDER BY name")?.query_map([],|r|Ok(json!({"name":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"consecutive_failures":r.get::<_,i64>(2)?,"last_checked_ms":r.get::<_,i64>(3)?,"last_success_ms":r.get::<_,Option<i64>>(4)?,"detail":r.get::<_,String>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let events=self.db.prepare("SELECT id,source,kind,state,attempts,last_error FROM event_inbox WHERE state!='done' ORDER BY created_ms LIMIT 100")?.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"source":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"state":r.get::<_,String>(3)?,"attempts":r.get::<_,i64>(4)?,"last_error":r.get::<_,Option<String>>(5)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        // Existing legacy drafts stay owned by their original tables and are read as
        // a projection. Do not migrate or reset an uncertain historical delivery.
        Ok(
            json!({"generated_at":Utc::now().to_rfc3339(),"operations":actions,"receipts":receipts,"projects":self.project_list()?["projects"],"shipments":self.shipment_list()?["shipments"],"sources":sources,"health":health,"pending_events":events,"legacy_messages":self.message_list()?["drafts"]}),
        )
    }
    pub fn briefing(&self) -> Result<Value> {
        let activity = self.activity()?;
        let projects: Vec<_> = activity["projects"]
            .as_array()
            .context("projects")?
            .iter()
            .filter(|p| p["project"]["status"] != "completed" && p["project"]["status"] != "paused")
            .cloned()
            .collect();
        let exceptions: Vec<_> = activity["operations"]
            .as_array()
            .context("operations")?
            .iter()
            .filter(|p| {
                matches!(
                    p["state"].as_str(),
                    Some("failed" | "uncertain" | "partial")
                )
            })
            .cloned()
            .collect();
        let shipments: Vec<_> = activity["shipments"]
            .as_array()
            .context("shipments")?
            .iter()
            .filter(|p| {
                matches!(
                    p["state"].as_str(),
                    Some("delayed" | "exception" | "out_for_delivery")
                )
            })
            .cloned()
            .collect();
        let health: Vec<_> = activity["health"]
            .as_array()
            .context("health")?
            .iter()
            .filter(|p| p["state"] != "healthy")
            .cloned()
            .collect();
        Ok(
            json!({"as_of":activity["generated_at"],"projects":projects,"action_exceptions":exceptions,"shipment_exceptions":shipments,"connector_exceptions":health,"sources":activity["sources"],"pending_events":activity["pending_events"],"presentation":"one concise exception-focused briefing; state stale/missing sources, do not infer an empty inbox/calendar"}),
        )
    }
    pub async fn refresh_google(&self) -> Result<()> {
        let now = Utc::now();
        let settings = std::fs::read(self.root.join("extensions/personal-ops/service.json"))
            .unwrap_or_default();
        let config: Value = serde_json::from_slice(&settings).unwrap_or(json!({}));
        let zone = config["timezone"]
            .as_str()
            .unwrap_or("America/Los_Angeles")
            .parse::<chrono_tz::Tz>()?;
        let date = now.with_timezone(&zone).date_naive();
        use chrono::TimeZone;
        let start = zone
            .from_local_datetime(&date.and_hms_opt(0, 0, 0).context("midnight")?)
            .earliest()
            .context("local midnight unavailable")?;
        let end = zone
            .from_local_datetime(
                &date
                    .succ_opt()
                    .context("next day")?
                    .and_hms_opt(0, 0, 0)
                    .context("midnight")?,
            )
            .earliest()
            .context("next midnight unavailable")?;
        let calendar=outbox::google(&self.root,"calendar","calendar.events.list",json!({"calendarId":"primary","timeMin":start.to_rfc3339(),"timeMax":end.to_rfc3339(),"singleEvents":true,"orderBy":"startTime","maxResults":250}),None,"read-calendar").await;
        let calendar_result = self.capture_source("calendar_today", calendar);
        let email=outbox::google(&self.root,"gmail","gmail.users.messages.list",json!({"userId":"me","q":"in:inbox is:unread (is:important OR is:starred)","maxResults":30}),None,"read-email").await;
        let email = match email {
            Ok(mut list) => {
                if let Some(ids) = list["messages"].as_array() {
                    let ids = ids.clone();
                    let mut messages = Vec::new();
                    for item in ids {
                        messages.push(outbox::google(&self.root,"gmail","gmail.users.messages.get",json!({"userId":"me","id":item["id"],"format":"metadata","metadataHeaders":["Subject","From","Date"]}),None,"read-email").await?);
                    }
                    list["messages"] = json!(messages);
                }
                Ok(list)
            }
            Err(e) => Err(e),
        };
        let email_result = self.capture_source("important_email", email);
        let invitations=outbox::google(&self.root,"calendar","calendar.events.list",json!({"calendarId":"primary","timeMin":now.to_rfc3339(),"timeMax":(now+chrono::Duration::days(90)).to_rfc3339(),"singleEvents":true,"orderBy":"startTime","maxResults":250}),None,"read-invitations").await.map(|v|json!({"events":v["items"].as_array().into_iter().flatten().filter(|e|e["attendees"].as_array().is_some_and(|a|a.iter().any(|a|matches!(a["responseStatus"].as_str(),Some("needsAction"|"declined"|"tentative"))))).cloned().collect::<Vec<_>>(),"truncated":v.get("nextPageToken").is_some(),"window_days":90}));
        let invitations_result = self.capture_source("pending_invitations", invitations);
        calendar_result.and(email_result).and(invitations_result)
    }
    fn capture_source(&self, name: &str, result: Result<Value>) -> Result<()> {
        match result {
            Ok(v) => {
                self.snapshot(name, &v, None)?;
                self.health_record(name, "healthy", "read verified")?;
            }
            Err(e) => {
                let message = e.to_string();
                let state = if message.to_lowercase().contains("scope")
                    || message.to_lowercase().contains("keychain")
                    || message.to_lowercase().contains("keyring")
                    || message.contains("401")
                    || message.contains("403")
                {
                    "authentication_required"
                } else {
                    "temporary_outage"
                };
                self.snapshot(name, &json!({}), Some(&message))?;
                self.health_record(name, state, &message)?;
                return Err(e);
            }
        }
        Ok(())
    }
    pub async fn refresh_github(&self) -> Result<()> {
        let out = outbox::output(
            std::path::Path::new("/opt/homebrew/bin/gh"),
            &["api".into(), "notifications?all=false&per_page=50".into()],
        )
        .await;
        let result = out.and_then(|o| {
            ensure!(o.status.success(), "GitHub notifications unavailable");
            Ok(serde_json::from_slice::<Value>(&o.stdout)?)
        });
        self.capture_source("github", result)
    }
    pub fn refresh_cron(&self) -> Result<()> {
        let path = self.root.join("data/cron/jobs.db");
        if !path.exists() {
            return self.snapshot(
                "scheduled_jobs",
                &json!({}),
                Some("scheduler database path not found"),
            );
        }
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut rows=db.prepare("SELECT id,name,next_run,last_status,last_output FROM cron_jobs ORDER BY next_run LIMIT 100")?.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,Option<String>>(1)?,"next_run":r.get::<_,String>(2)?,"last_status":r.get::<_,Option<String>>(3)?,"last_output":r.get::<_,Option<String>>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for job in &mut rows {
            let statuses = db
                .prepare(
                    "SELECT status FROM cron_runs WHERE job_id=?1 ORDER BY started_at DESC LIMIT 3",
                )?
                .query_map([job["id"].as_str().context("job id")?], |r| {
                    r.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            job["repeated_failure"] =
                json!(statuses.len() == 3 && statuses.iter().all(|s| s == "error"));
        }
        self.snapshot("scheduled_jobs", &json!({"jobs":rows}), None)
    }
    pub async fn process_events(&self) -> Result<Value> {
        let now = Utc::now().timestamp_millis();
        // A dead worker lease becomes retryable. These handlers only refresh source
        // reads; they never replay a communication or Calendar mutation.
        self.db.execute(
            "UPDATE event_inbox SET state='pending' WHERE state='processing' AND next_ms<?1",
            [now],
        )?;
        let rows=self.db.prepare("SELECT id,source,attempts FROM event_inbox WHERE state='pending' AND next_ms<=?1 ORDER BY created_ms LIMIT 20")?.query_map([now],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,u32>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, source, attempts) in &rows {
            if self.db.execute("UPDATE event_inbox SET state='processing',next_ms=?2 WHERE id=?1 AND state='pending'",params![id,now+300_000])?!=1{continue;}
            let result = match source.as_str() {
                "gmail" => {
                    let read = self.refresh_google().await;
                    let shipments = self.shipment_discover().await;
                    read.and(shipments.map(|_| ()))
                }
                "calendar" => self.refresh_google().await,
                "reminders" => self.refresh_reminders().await,
                "github" => self.refresh_github().await,
                "runtime" => self.refresh_cron(),
                _ => Ok(()),
            };
            let state = if result.is_ok() {
                "done"
            } else if *attempts >= 4 {
                "dead_letter"
            } else {
                "pending"
            };
            let error = result.err().map(|e| e.to_string());
            self.db.execute("UPDATE event_inbox SET state=?2,attempts=attempts+1,next_ms=?3,last_error=?4 WHERE id=?1",params![id,state,now+(30_000_i64*(1_i64<<attempts.min(&10))),error])?;
        }
        Ok(json!({"processed":rows.len()}))
    }
    pub async fn alert_owner(&self, key: &str, text: &str) -> Result<bool> {
        let settings = self.root.join("extensions/personal-ops/service.json");
        let config: Value = serde_json::from_slice(&std::fs::read(settings)?)?;
        let recipient = config["telegram_recipient"]
            .as_str()
            .context("owner Telegram recipient not configured")?;
        let channel = config["telegram_channel"]
            .as_str()
            .context("owner Telegram channel not configured")?;
        // This is an at-most-once alert claim. Uncertain notifications are visible in
        // activity, never looped into repeated failure notifications.
        if self.db.execute(
            "INSERT OR IGNORE INTO alert_receipts VALUES(?1,'uncertain',?2)",
            params![key, Utc::now().timestamp_millis()],
        )? == 0
        {
            return Ok(false);
        }
        let exe = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?)
            .join(".cargo/bin/zeroclaw");
        let result = outbox::output(
            &exe,
            &[
                "--config-dir".into(),
                self.root.to_string_lossy().into_owned(),
                "channel".into(),
                "send".into(),
                text.into(),
                "--channel-id".into(),
                channel.into(),
                "--recipient".into(),
                recipient.into(),
            ],
        )
        .await?;
        ensure!(result.status.success(), "owner notification uncertain");
        self.db.execute(
            "UPDATE alert_receipts SET state='submitted' WHERE key=?1",
            [key],
        )?;
        Ok(true)
    }
    pub async fn notify_exceptions(&self) -> Result<()> {
        let activity = self.activity()?;
        for shipment in activity["shipments"].as_array().context("shipments")? {
            if matches!(
                shipment["state"].as_str(),
                Some("delayed" | "exception" | "delivered" | "out_for_delivery")
            ) {
                let key = format!("shipment:{}:{}", shipment["shipment_id"], shipment["state"]);
                let text = format!(
                    "Package update: {} — {}. {}",
                    shipment["label"].as_str().unwrap_or("Package"),
                    shipment["state"].as_str().unwrap_or("updated"),
                    shipment["tracking_url"].as_str().unwrap_or("")
                );
                self.alert_owner(&key, &text).await?;
            }
        }
        for operation in activity["operations"].as_array().context("operations")? {
            if matches!(
                operation["state"].as_str(),
                Some("failed" | "uncertain" | "partial")
            ) {
                self.alert_owner(&format!("operation:{}:{}",operation["operation_id"],operation["state"]), &format!("ZeroClaw action needs attention: {} ({}). Review the durable receipt in the activity dashboard before retrying.",operation["review"]["title"].as_str().unwrap_or("action"),operation["state"].as_str().unwrap_or("uncertain"))).await?;
            }
        }
        for source in activity["sources"].as_array().context("sources")? {
            if source["source"] == "scheduled_jobs" {
                for job in source["data"]["jobs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|j| j["repeated_failure"] == true)
                {
                    self.alert_owner(&format!("automation:{}",job["id"]),&format!("ZeroClaw automation failed three times: {}. See the dashboard for the latest evidence.",job["name"].as_str().unwrap_or("scheduled job"))).await?;
                }
            }
        }
        for connector in activity["health"].as_array().context("health")? {
            if connector["consecutive_failures"]
                .as_i64()
                .is_some_and(|n| n >= 3)
                && connector["state"] != "not_configured"
            {
                let key = format!(
                    "connector:{}:{}",
                    connector["name"], connector["last_success_ms"]
                );
                self.alert_owner(&key,&format!("ZeroClaw needs attention: {} has failed repeatedly ({}). Details are in the activity dashboard.",connector["name"].as_str().unwrap_or("connector"),connector["state"].as_str().unwrap_or("unavailable"))).await?;
            }
        }
        Ok(())
    }
}

impl Ops {
    pub async fn refresh_reminders(&self) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new(
            self.root
                .join("extensions/reminders-manager/zeroclaw-reminders-manager"),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
        let mut stdin = child.stdin.take().context("Reminders stdin")?;
        stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"list\",\"arguments\":{\"include_completed\":false,\"limit\":200}}}\n").await?;
        drop(stdin);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
                .await??;
        let line = output
            .stdout
            .split(|b| *b == b'\n')
            .find(|s| !s.is_empty())
            .context("no Reminders response")?;
        let response: Value = serde_json::from_slice(line)?;
        if response["result"]["isError"] == true {
            let message = response["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or("Reminders access unavailable");
            self.health_record("reminders", "temporary_outage", message)?;
            anyhow::bail!("Reminders access unavailable: {message}");
        }
        let value: Value = serde_json::from_str(
            response["result"]["content"][0]["text"]
                .as_str()
                .context("Reminders payload missing")?,
        )?;
        let reminders = value["reminders"]
            .as_array()
            .context("Reminders list missing")?;
        let overdue: Vec<_> = reminders
            .iter()
            .filter(|r| {
                r["due"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|d| d < chrono::Utc::now())
            })
            .cloned()
            .collect();
        self.snapshot(
            "overdue_reminders",
            &json!({"reminders":overdue,"upcoming_due":reminders.iter().filter(|r|r["due"].as_str().and_then(|s|chrono::DateTime::parse_from_rfc3339(s).ok()).is_some_and(|d|d>=chrono::Utc::now())).cloned().collect::<Vec<_>>(),"truncated":reminders.len()==200}),
            None,
        )?;
        self.health_record(
            "reminders",
            "healthy",
            "read-only overdue reminder refresh verified",
        )
    }
}

impl Ops {
    pub fn reminder_due_events(&self) -> Result<()> {
        let snapshot: Option<String> = self
            .db
            .query_row(
                "SELECT payload FROM source_snapshots WHERE source='overdue_reminders'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(snapshot) = snapshot {
            let value: Value = serde_json::from_str(&snapshot)?;
            for reminder in value["upcoming_due"].as_array().into_iter().flatten() {
                if reminder["due"]
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .is_some_and(|d| d <= Utc::now())
                {
                    self.event_ingest(&json!({"event_id":format!("due:{}:{}",reminder["id"],reminder["due"]),"source":"reminders","kind":"reminder_overdue","payload":{"reminder_id":reminder["id"]}}))?;
                }
            }
        }
        Ok(())
    }
}

impl Ops {
    /// Synthetic probes send no messages and contain no personal query data.
    pub async fn refresh_routing_health(&self) -> Result<()> {
        let settings: Value = serde_json::from_slice(&std::fs::read(
            self.root.join("extensions/personal-ops/service.json"),
        )?)?;
        let config = outbox::config(&self.root)?;
        let channel = settings["telegram_channel"]
            .as_str()
            .context("Telegram route not configured")?;
        let route_ok = config["agents"]["main"]["channels"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == channel))
            && settings["telegram_recipient"]
                .as_str()
                .is_some_and(|r| r.split(':').all(|p| p.parse::<i64>().is_ok()));
        if !route_ok {
            self.health_record(
                "conversation_routing",
                "unsupported",
                "Owner channel or exact recipient no longer matches configured main route",
            )?;
        } else {
            let cli = std::path::PathBuf::from(std::env::var_os("HOME").context("HOME")?)
                .join(".cargo/bin/zeroclaw");
            let result = outbox::output(
                &cli,
                &[
                    "--config-dir".into(),
                    self.root.to_string_lossy().into_owned(),
                    "channel".into(),
                    "doctor".into(),
                ],
            )
            .await;
            let healthy = result.is_ok_and(|o| {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout).contains("0 unhealthy, 0 timed out")
            });
            self.health_record("conversation_routing",if healthy{"healthy"}else{"temporary_outage"},if healthy{"Configured owner route and native channel health verified; no test message sent"}else{"Native channel health check failed; inspect channel doctor"})?;
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        for (name, url, marker) in [
            (
                "search_bing",
                "https://www.bing.com/search?format=rss&q=rust+language",
                "<rss",
            ),
            (
                "search_duckduckgo",
                "https://html.duckduckgo.com/html/?q=rust+language",
                "result__a",
            ),
        ] {
            let response = client.get(url).send().await;
            let healthy = match response {
                Ok(r)
                    if r.status().as_u16() == 200
                        && r.content_length().is_none_or(|n| n < 2_000_000) =>
                {
                    r.text().await.is_ok_and(|s| s.contains(marker))
                }
                _ => false,
            };
            self.health_record(
                name,
                if healthy {
                    "healthy"
                } else {
                    "temporary_outage"
                },
                if healthy {
                    "Synthetic public query returned recognized search results"
                } else {
                    "Synthetic search blocked/unavailable; configured failover may be used"
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn event_dedup_and_failed_refresh_preserve_truth() -> Result<()> {
        let t = tempfile::tempdir()?;
        let o = Ops::open(t.path())?;
        let a = json!({"event_id":"fixture","source":"calendar","kind":"calendar_changed"});
        assert_eq!(o.event_ingest(&a)?["duplicate_prevented"], false);
        assert_eq!(o.event_ingest(&a)?["duplicate_prevented"], true);
        o.snapshot("calendar", &json!({"events":["fixture"]}), None)?;
        o.snapshot("calendar", &json!({}), Some("outage"))?;
        let v = o.activity()?;
        assert_eq!(v["sources"][0]["data"]["events"][0], "fixture");
        assert_eq!(v["sources"][0]["error"], "outage");
        Ok(())
    }
}
