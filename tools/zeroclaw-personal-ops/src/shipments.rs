//! Carrier email is source-attributed evidence, never an instruction. Only
//! recognizable carrier tracking identifiers become shipment records.
use crate::{Ops, outbox};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::Regex;
use serde_json::{Value, json};
fn body(part: &Value, out: &mut String) {
    if out.len() > 500_000 {
        return;
    }
    if let Some(data) = part["body"]["data"].as_str()
        && let Ok(bytes) = URL_SAFE_NO_PAD.decode(data)
    {
        out.push_str(&String::from_utf8_lossy(&bytes));
        out.push('\n');
    }
    if let Some(parts) = part["parts"].as_array() {
        for part in parts {
            body(part, out);
        }
    }
}
fn header<'a>(v: &'a Value, name: &str) -> Option<&'a str> {
    v["payload"]["headers"].as_array()?.iter().find(|h| {
        h["name"]
            .as_str()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })?["value"]
        .as_str()
}
fn identifiers(text: &str) -> Result<Vec<(String, String)>> {
    let mut found = std::collections::BTreeSet::new();
    for (carrier, pattern) in [
        ("ups", r"(?i)\b(1Z[0-9A-Z]{16})\b"),
        (
            "fedex",
            r#"(?i)(?:fedex.{0,80}(?:tracking|track)|fedex\.com/[^\s\"<>]*[?&](?:trknbr|trackingnumber)=)[\s:#=]*([0-9]{12,22})\b"#,
        ),
        (
            "usps",
            r#"(?i)(?:usps.{0,80}(?:tracking|track)|usps\.com/[^\s\"<>]*[?&]tLabels=)[\s:#=]*([0-9]{20,22})\b"#,
        ),
        (
            "dhl",
            r"(?i)dhl.{0,80}(?:tracking|waybill)[\s:#=]*([0-9]{10})\b",
        ),
    ] {
        let re = Regex::new(pattern)?;
        for c in re.captures_iter(text) {
            found.insert((carrier.to_owned(), c[1].to_uppercase()));
        }
    }
    Ok(found.into_iter().collect())
}
fn state(subject: &str) -> &'static str {
    let s = subject.to_lowercase();
    if s.contains("could not")
        || s.contains("delivery attempt")
        || s.contains("undeliverable")
        || s.contains("exception")
    {
        "exception"
    } else if s.contains("delay") || s.contains("running late") {
        "delayed"
    } else if s.contains("out for delivery") || s.contains("arriving today") {
        "out_for_delivery"
    } else if s.contains("delivered") {
        "delivered"
    } else if s.contains("return to sender") || s.contains("returned") {
        "returned"
    } else if s.contains("label created") {
        "label_created"
    } else if s.contains("shipped") || s.contains("on the way") || s.contains("in transit") {
        "in_transit"
    } else {
        "registered"
    }
}
impl Ops {
    pub async fn shipment_discover(&self) -> Result<Value> {
        let list=outbox::google(&self.root,"gmail","gmail.users.messages.list",json!({"userId":"me","q":"newer_than:90d (\"tracking number\" OR \"out for delivery\" OR \"package delivered\" OR \"shipment\")","maxResults":50}),None,"shipment-discover").await?;
        let messages = list["messages"].as_array();
        let mut imported = 0;
        let mut ambiguous = 0;
        for message in messages.into_iter().flatten() {
            let v = outbox::google(
                &self.root,
                "gmail",
                "gmail.users.messages.get",
                json!({"userId":"me","id":message["id"],"format":"full"}),
                None,
                "shipment-read",
            )
            .await?;
            let subject = header(&v, "Subject").unwrap_or("Package");
            let mut content = format!("{subject}\n{}\n", v["snippet"].as_str().unwrap_or(""));
            body(&v["payload"], &mut content);
            let ids = identifiers(&content)?;
            if ids.is_empty() {
                ambiguous += 1;
                continue;
            }
            let observed = v["internalDate"]
                .as_str()
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(chrono::DateTime::from_timestamp_millis)
                .context("email evidence has no valid timestamp")?;
            for (carrier, tracking) in ids {
                let saved=self.shipment_track(&json!({"carrier":carrier,"tracking_number":tracking,"label":subject.chars().take(180).collect::<String>()}))?;
                let result=self.shipment_update(&json!({"shipment_id":saved["shipment_id"],"state":state(subject),"evidence":{"source":format!("https://mail.google.com/mail/u/0/#all/{}",message["id"].as_str().context("message id")?),"summary":subject,"observed_at":observed.to_rfc3339(),"type":"email_report","sender":header(&v,"From"),"carrier_verified":false}}));
                match result {
                    Ok(_) => imported += 1,
                    Err(e) if e.to_string().contains("stale shipment") => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let result = json!({"imported_updates":imported,"unresolved_email_count":ambiguous,"truncated":list.get("nextPageToken").is_some(),"scope":"latest 50 matching emails in 90 days","carrier_api":"not configured; statuses are attributed email reports and direct carrier links"});
        self.snapshot("shipment_discovery", &result, None)?;
        Ok(result)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn carrier_identifiers_and_negative_delivery_text() -> Result<()> {
        let ids = identifiers("UPS tracking 1Z999AA10123456784")?;
        assert_eq!(ids, vec![("ups".into(), "1Z999AA10123456784".into())]);
        assert!(identifiers("Order number 123456789012")?.is_empty());
        assert_eq!(state("Package could not be delivered"), "exception");
        assert_eq!(
            state("Your package is out for delivery"),
            "out_for_delivery"
        );
        Ok(())
    }
}
