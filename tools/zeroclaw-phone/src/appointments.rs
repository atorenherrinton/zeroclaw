//! Narrow inbound scheduling boundary. The remote voice model supplies a
//! proposal; the local scheduler owns verification, calendar access and receipts.

use crate::common::{SafeResult, check};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin};

pub const INBOUND_INSTRUCTIONS: &str = "\nRuntime appointment capability: for this inbound call only, the earlier message-only and no-callback restrictions have one narrow exception. A business asking to reschedule an existing appointment may propose a tentative replacement time through tentatively_reschedule_appointment. Ask for its business name, branch street address and city, existing appointment date/time and proposed date/time with timezone. Read back the proposed time and obtain confirmation before the tool. Supply only public business lookup details, never owner information, in the business fields. Never search contacts or expose owner calendar entries. The tool locally matches the incoming phone number to a public Apple Maps listing and checks the calendar. Do not claim those checks passed before a successful result. Only a tentative_hold_created result permits saying the time is penciled in tentatively, pending owner review, and that the owner will call back to reschedule if that date does not work. Keep the original appointment pending owner review; do not claim it was cancelled or definitively moved. If the tool cannot verify the arrangement or its result is uncertain, take a message with the proposed date and callback number. Never infer availability, probe alternative times, retry scheduling in this call, or place an outbound call. Recording objection still immediately takes priority.\n";

pub const TOOL_NAME: &str = "tentatively_reschedule_appointment";
pub const MAX_ARGUMENT_BYTES: usize = 2048;

// Deliberately no Debug: appointment dates and caller statements are private.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub business_name: String,
    pub business_address: String,
    pub original_start: String,
    pub proposed_start: String,
    pub caller_confirmed: bool,
}

impl Request {
    pub fn parse(value: Value) -> SafeResult<Self> {
        let request: Self =
            serde_json::from_value(value).map_err(|_| "appointment_arguments_invalid")?;
        for text in [&request.business_name, &request.business_address] {
            check(
                (3..=200).contains(&text.len())
                    && !text.chars().any(char::is_control)
                    && !text.contains("://"),
                "appointment_business_invalid",
            )?;
        }
        for text in [&request.original_start, &request.proposed_start] {
            check(
                text.len() <= 40 && chrono::DateTime::parse_from_rfc3339(text).is_ok(),
                "appointment_time_invalid",
            )?;
        }
        check(request.caller_confirmed, "appointment_confirmation_missing")?;
        Ok(request)
    }
}

pub type SchedulingFuture = Pin<Box<dyn Future<Output = Value> + Send>>;

pub trait AppointmentScheduler: Send + Sync {
    /// Implementations receive their call identity from authenticated ingress,
    /// never from these model arguments. Dropping the future stops new work.
    fn schedule(&self, request: Request) -> SchedulingFuture;
}

pub fn tool_definition() -> Value {
    json!({
        "type":"function", "name":TOOL_NAME,
        "description":"For an inbound business asking to reschedule an existing appointment only. First collect its business name and full branch address/city, the current appointment date/time, and the proposed new date/time including timezone. Read the proposed date/time back and obtain caller confirmation before invoking. The local service independently matches this call's incoming number against a public Apple Maps listing, matches the existing calendar appointment, checks conflicts, and may create one tentative hold with no invitations. It preserves the original appointment for owner review. Do not claim verification, availability or a calendar change until the tool explicitly succeeds. On any unsuccessful or uncertain result, take a message and promise no booking. Never reveal other calendar events or propose times by probing this tool.",
        "parameters":{
            "type":"object","additionalProperties":false,
            "properties":{
                "business_name":{"type":"string","maxLength":200},
                "business_address":{"type":"string","maxLength":200,"description":"Public branch street address and city supplied by the caller. Never send owner information or appointment details in this field."},
                "original_start":{"type":"string","description":"Existing appointment start, supplied by the caller, in RFC3339 with an explicit UTC offset."},
                "proposed_start":{"type":"string","description":"Proposed appointment start, read back to the caller, in RFC3339 with an explicit UTC offset."},
                "caller_confirmed":{"type":"boolean","enum":[true]}
            },
            "required":["business_name","business_address","original_start","proposed_start","caller_confirmed"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn request() -> Value {
        json!({"business_name":"Example Clinic","business_address":"123 Example St, Example City",
            "original_start":"2026-10-01T09:00:00-07:00","proposed_start":"2026-10-02T10:00:00-07:00","caller_confirmed":true})
    }

    #[test]
    fn proposal_cannot_supply_identity_credentials_or_ambiguous_dates() {
        assert!(Request::parse(request()).is_ok());
        for (key, value) in [
            ("caller_number", json!("+12065550100")),
            ("calendar_id", json!("primary")),
            ("owner_authorized", json!(true)),
            ("original_start", json!("tomorrow at nine")),
            ("caller_confirmed", json!(false)),
            ("business_address", json!("https://caller.invalid")),
        ] {
            let mut data = request();
            data[key] = value;
            assert!(Request::parse(data).is_err(), "{key}");
        }
    }
}
