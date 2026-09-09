//! Project the installed Google notification bridge's live watch receipts into
//! connector health. The bridge remains the owner of watch renewal and state.
use crate::Ops;
use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use std::time::Duration;

fn watch_health(status: &Value, source: &str, now: i64) -> (&'static str, &'static str) {
    let error_key = format!("{source}_error");
    if status[&error_key].as_str().is_some_and(|s| !s.is_empty()) {
        return (
            "temporary_outage",
            "Google watch renewal failed; polling remains available.",
        );
    }
    let expiration = if source == "gmail" {
        status["gmail_expiration_ms"].as_i64().unwrap_or(0)
    } else {
        status["channels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["resourceId"].as_str().is_some_and(|id| !id.is_empty()))
            .filter_map(|c| c["expiration"].as_str()?.parse::<i64>().ok())
            .max()
            .unwrap_or(0)
    };
    if expiration <= now {
        return (
            "temporary_outage",
            "Google watch is missing or expired; polling remains available.",
        );
    }
    (
        "healthy",
        "Notification receiver is running with an unexpired Google watch and automatic renewal.",
    )
}

impl Ops {
    pub async fn refresh_google_push_health(&self) -> Result<()> {
        let path = self.root.join("extensions/google-push/config.json");
        if !path.exists() {
            for name in ["gmail_push", "calendar_push"] {
                self.health_record(
                    name,
                    "not_configured",
                    "Google notification bridge is not configured; polling remains available.",
                )?;
            }
            return Ok(());
        }
        // Keep this local: the credential must never follow a configured remote URL.
        let status = async {
            let config: Value = serde_json::from_slice(&std::fs::read(path)?)?;
            let secret = config["Secret"]
                .as_str()
                .ok_or_else(|| anyhow::Error::msg("bridge credential missing"))?;
            let response = reqwest::Client::builder()
                .timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::none())
                .build()?
                .get("http://127.0.0.1:3336/google-notifications/status")
                .bearer_auth(secret)
                .send()
                .await?
                .error_for_status()?;
            Ok::<Value, anyhow::Error>(response.json().await?)
        }
        .await;
        for source in ["gmail", "calendar"] {
            let (state, detail) = match &status {
                Ok(status) => watch_health(status, source, Utc::now().timestamp_millis()),
                Err(_) => (
                    "temporary_outage",
                    "Configured Google notification receiver is unavailable; polling remains available.",
                ),
            };
            self.health_record(&format!("{source}_push"), state, detail)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_live_unexpired_watch_receipts_are_healthy() {
        let live = json!({"gmail_expiration_ms":2000,"channels":[{"resourceId":"bound","expiration":"2000"}]});
        for source in ["gmail", "calendar"] {
            assert_eq!(watch_health(&live, source, 1000).0, "healthy");
            assert_eq!(watch_health(&live, source, 3000).0, "temporary_outage");
            assert_eq!(watch_health(&json!({}), source, 1000).0, "temporary_outage");
            let mut failed = live.clone();
            failed[format!("{source}_error")] = json!("renewal failed");
            assert_eq!(watch_health(&failed, source, 1000).0, "temporary_outage");
        }
    }
    #[test]
    fn pending_calendar_registration_is_not_a_live_watch() {
        assert_eq!(
            watch_health(
                &json!({"channels":[{"id":"pending","expiration":"2000"}]}),
                "calendar",
                1000
            )
            .0,
            "temporary_outage"
        );
    }
}
