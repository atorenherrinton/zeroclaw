//! Existing-group discovery and structured `imsg` delivery.
//!
//! Group lookup is read-only. A prepared group destination binds the local chat
//! rowid, portable identifiers, iMessage service, and normalized external
//! participant snapshot. The snapshot is re-read before any delivery claim.
use crate::{GroupTarget, Item, digest, recipient};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const IMSG: &str = "/opt/homebrew/bin/imsg";
const GROUP_PREFIX: &str = "imessage-group:";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SendOutcome {
    Submitted,
    NotStarted(String),
    Uncertain(String),
}

pub(crate) fn normalize_handle(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let value = trimmed
        .strip_prefix("tel:")
        .or_else(|| trimmed.strip_prefix("mailto:"))
        .unwrap_or(trimmed);
    if value.contains('@') {
        let normalized = value.to_ascii_lowercase();
        ensure!(recipient(&normalized), "invalid iMessage participant");
        return Ok(normalized);
    }
    ensure!(
        value.starts_with('+')
            && value
                .bytes()
                .skip(1)
                .all(|b| b.is_ascii_digit() || b" ()-.".contains(&b)),
        "group participants must be exact E.164 numbers or iMessage email addresses"
    );
    let digits: String = value
        .bytes()
        .filter(|b| b.is_ascii_digit())
        .map(char::from)
        .collect();
    let normalized = format!("+{digits}");
    ensure!(recipient(&normalized), "invalid iMessage participant");
    Ok(normalized)
}

pub(crate) fn normalize_participants<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<String>> {
    let mut set = BTreeSet::new();
    for value in values {
        ensure!(
            set.insert(normalize_handle(value)?),
            "duplicate group participant"
        );
    }
    ensure!(
        (2..=10).contains(&set.len()),
        "an existing group must have 2 to 10 external participants"
    );
    Ok(set.into_iter().collect())
}

fn participant_hash(participants: &[String]) -> Result<String> {
    Ok(digest(&serde_json::to_vec(participants)?))
}

impl GroupTarget {
    pub(crate) fn token(&self) -> Result<String> {
        Ok(format!(
            "{GROUP_PREFIX}{}:{}",
            self.chat_id,
            participant_hash(&self.participants)?
        ))
    }
}

fn clean_error(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).replace(['\r', '\n'], " ");
    let text = text.trim();
    if text.is_empty() {
        "imsg returned no diagnostic".to_owned()
    } else {
        text.chars().take(500).collect()
    }
}

fn parse_group(value: &Value) -> Result<GroupTarget> {
    let chat_id = value["id"].as_i64().context("group chat id missing")?;
    ensure!(chat_id > 0, "invalid group chat id");
    let chat_identifier = value["identifier"]
        .as_str()
        .context("group identifier missing")?
        .to_owned();
    let chat_guid = value["guid"]
        .as_str()
        .context("group guid missing")?
        .to_owned();
    let service = value["service"]
        .as_str()
        .context("group service missing")?
        .to_owned();
    ensure!(
        value["is_group"] == true && (chat_identifier.contains(";+;") || chat_guid.contains(";+;")),
        "selected conversation is not a group"
    );
    ensure!(
        ["imessage", "imessagelite", "sms", "rcs"].contains(&service.to_ascii_lowercase().as_str()),
        "selected group has an unsupported Messages service"
    );
    ensure!(
        !chat_identifier.is_empty() && !chat_guid.is_empty(),
        "group identifiers must be nonempty"
    );
    let participants = value["participants"]
        .as_array()
        .context("group participants missing")?;
    let participants = normalize_participants(
        participants
            .iter()
            .map(|v| v.as_str().context("group participant must be a string"))
            .collect::<Result<Vec<_>>>()?,
    )?;
    let name = value
        .get("display_name")
        .or_else(|| value.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(256)
        .collect();
    Ok(GroupTarget {
        chat_id,
        chat_identifier,
        chat_guid,
        service,
        name,
        participants,
    })
}

fn parse_ndjson(bytes: &[u8]) -> Result<Vec<Value>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).context("invalid imsg JSON output"))
        .collect()
}

fn recent_group_values(limit: Option<usize>) -> Result<Vec<Value>> {
    if let Some(limit) = limit {
        ensure!((1..=500).contains(&limit), "limit must be 1 to 500");
    }
    let mut command = Command::new(IMSG);
    command.arg("chats");
    if let Some(limit) = limit {
        command.args(["--limit", &limit.to_string()]);
    }
    let output = command
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .context("could not start read-only imsg chat lookup")?;
    ensure!(
        output.status.success(),
        "read-only iMessage group lookup failed: {}",
        clean_error(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        })
    );
    parse_ndjson(&output.stdout)
}

pub(crate) fn group_detail(chat_id: i64) -> Result<GroupTarget> {
    ensure!(chat_id > 0, "chat_id must be positive");
    let output = Command::new(IMSG)
        .args(["group", "--chat-id", &chat_id.to_string(), "--json"])
        .stdin(Stdio::null())
        .output()
        .context("could not start read-only imsg group lookup")?;
    ensure!(
        output.status.success(),
        "read-only iMessage group lookup failed: {}",
        clean_error(if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        })
    );
    let values = parse_ndjson(&output.stdout)?;
    ensure!(
        values.len() == 1,
        "group lookup returned an unexpected result count"
    );
    parse_group(&values[0])
}

pub(crate) fn select_exact_groups(
    values: &[Value],
    requested: &[String],
) -> Result<Vec<GroupTarget>> {
    let requested = normalize_participants(requested.iter().map(String::as_str))?;
    let mut matches = Vec::new();
    for value in values {
        if value["is_group"] != true {
            continue;
        }
        let group = match parse_group(value) {
            Ok(group) => group,
            Err(_) => continue,
        };
        if group.participants == requested {
            matches.push(group);
        }
    }
    matches.sort_by_key(|g| g.chat_id);
    Ok(matches)
}

fn search_limit(args: &Value) -> Result<Option<usize>> {
    let limit = args
        .get("limit")
        .map(|value| value.as_u64().context("limit must be an integer"))
        .transpose()?
        .map(|value| value as usize);
    if let Some(limit) = limit {
        ensure!((1..=500).contains(&limit), "limit must be 1 to 500");
    }
    Ok(limit)
}

pub(crate) fn search_groups(args: &Value) -> Result<Value> {
    let participants = args["participants"]
        .as_array()
        .context("participants must be an array of exact handles")?
        .iter()
        .map(|v| {
            v.as_str()
                .context("participant must be a string")
                .map(str::to_owned)
        })
        .collect::<Result<Vec<_>>>()?;
    let normalized = normalize_participants(participants.iter().map(String::as_str))?;
    let limit = search_limit(args)?;
    let values = recent_group_values(limit)?;
    let matches = select_exact_groups(&values, &normalized)?;
    let rendered = matches
        .iter()
        .map(|group| {
            Ok(json!({
                "chat_id":group.chat_id,
                "chat_identifier":group.chat_identifier,
                "chat_guid":group.chat_guid,
                "name":group.name,
                "service":group.service,
                "participants":group.participants,
                "group_token":group.token()?
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "requested_participants":normalized,
        "searched_chats":values.len(),
        "truncated":limit.is_some_and(|limit| values.len() == limit),
        "exhaustive":limit.is_none(),
        "matches":rendered,
        "match_count":rendered.len(),
        "selected":if rendered.len()==1 { Some(rendered[0].clone()) } else { None },
        "selection":"Use group_token only for the intended existing group. Participants are exact chat handles; resolve contact email/phone aliases when needed. The reported service is stored metadata; sending uses the existing conversation and its current transport. No group is created."
    }))
}

pub(crate) fn get_group(args: &Value) -> Result<Value> {
    let chat_id = args["chat_id"]
        .as_i64()
        .context("chat_id must be an integer")?;
    let group = group_detail(chat_id)?;
    Ok(json!({
        "chat_id":group.chat_id,
        "chat_identifier":group.chat_identifier,
        "chat_guid":group.chat_guid,
        "name":group.name,
        "service":group.service,
        "participants":group.participants,
        "group_token":group.token()?,
        "existing":true,
        "created":false
    }))
}

pub(crate) fn resolve_group_token(token: &str) -> Result<GroupTarget> {
    let raw = token
        .strip_prefix(GROUP_PREFIX)
        .context("invalid existing-group token")?;
    let (id, expected_hash) = raw
        .split_once(':')
        .context("invalid existing-group token")?;
    ensure!(
        expected_hash.len() == 64 && expected_hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid existing-group participant hash"
    );
    let group = group_detail(id.parse().context("invalid group chat id")?)?;
    ensure!(
        participant_hash(&group.participants)? == expected_hash.to_ascii_lowercase(),
        "existing group participants changed; search and review the group again"
    );
    Ok(group)
}

pub(crate) fn validate_group_snapshot(expected: &GroupTarget, current: &GroupTarget) -> Result<()> {
    ensure!(
        expected.chat_id == current.chat_id
            && expected.chat_identifier == current.chat_identifier
            && expected.chat_guid == current.chat_guid
            && expected.service == current.service
            && expected.participants == current.participants,
        "existing group identity or participants changed; search and prepare again"
    );
    Ok(())
}

pub(crate) fn send_params(item: &Item, path: Option<&PathBuf>) -> Value {
    let mut params = json!({"text":item.text,"transport":"applescript","allow_sms_fallback":false});
    if let Some(group) = &item.group {
        params["chat_id"] = json!(group.chat_id);
    } else {
        params["to"] = json!(item.recipient);
        params["service"] = json!("imessage");
    }
    if let Some(path) = path {
        params["file"] = json!(path);
    }
    params
}

fn rpc_error_summary(error: &Value) -> String {
    error["data"]["detail"]
        .as_str()
        .or_else(|| error["data"].as_str())
        .or_else(|| error["message"].as_str())
        .unwrap_or("imsg returned an unclassified delivery error")
        .chars()
        .take(500)
        .collect()
}

pub(crate) fn classify_rpc_response(value: &Value) -> SendOutcome {
    if value["result"]["ok"] == true {
        return SendOutcome::Submitted;
    }
    let error = &value["error"];
    let retry_safe = error["data"]["retry_safe"] == true;
    let disposition = error["data"]["disposition"].as_str().unwrap_or("");
    let summary = rpc_error_summary(error);
    let pre_dispatch =
        !error["data"].is_object() && matches!(error["code"].as_i64(), Some(-32602 | -32002));
    if (retry_safe && disposition == "not_started") || pre_dispatch {
        SendOutcome::NotStarted(summary)
    } else {
        SendOutcome::Uncertain(summary)
    }
}

pub(crate) async fn send_item(item: Item, path: Option<PathBuf>) -> SendOutcome {
    let request_id = uuid::Uuid::new_v4().to_string();
    let request = json!({
        "jsonrpc":"2.0",
        "id":request_id,
        "method":"send",
        "params":send_params(&item,path.as_ref())
    });
    let mut child = match tokio::process::Command::new(IMSG)
        .arg("rpc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => return SendOutcome::NotStarted(format!("could not start imsg: {error}")),
    };
    let Some(mut stdin) = child.stdin.take() else {
        return SendOutcome::NotStarted("could not open imsg request stream".to_owned());
    };
    let line = match serde_json::to_vec(&request) {
        Ok(mut bytes) => {
            bytes.push(b'\n');
            bytes
        }
        Err(error) => {
            return SendOutcome::NotStarted(format!("could not encode imsg request: {error}"));
        }
    };
    if let Err(error) = stdin.write_all(&line).await {
        return SendOutcome::Uncertain(format!("lost imsg request stream: {error}"));
    }
    if let Err(error) = stdin.shutdown().await {
        return SendOutcome::Uncertain(format!("could not close imsg request stream: {error}"));
    }
    drop(stdin);
    let output = match tokio::time::timeout(Duration::from_secs(45), child.wait_with_output()).await
    {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return SendOutcome::Uncertain(format!("imsg result unavailable: {error}"));
        }
        Err(_) => {
            return SendOutcome::Uncertain(
                "imsg send timed out; delivery may still have completed".to_owned(),
            );
        }
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value["id"].as_str() == Some(&request_id) {
            return classify_rpc_response(&value);
        }
    }
    SendOutcome::Uncertain(format!(
        "imsg returned no matching structured result: {}",
        clean_error(&output.stderr)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: i64, participants: &[&str]) -> Value {
        json!({
            "id":id,
            "identifier":format!("iMessage;+;chat{id}"),
            "guid":format!("iMessage;+;chat{id}"),
            "name":"Fixture group",
            "display_name":"Fixture group",
            "service":"iMessage",
            "is_group":true,
            "participants":participants
        })
    }

    #[test]
    fn group_search_defaults_to_exhaustive_and_validates_optional_limit() -> Result<()> {
        assert_eq!(search_limit(&json!({}))?, None);
        assert_eq!(search_limit(&json!({"limit": 500}))?, Some(500));
        for args in [
            json!({"limit": 0}),
            json!({"limit": 501}),
            json!({"limit": 1.5}),
        ] {
            assert!(search_limit(&args).is_err());
        }
        Ok(())
    }

    #[test]
    fn exact_group_selection_is_normalized_and_order_independent() -> Result<()> {
        let values = vec![
            group(7, &["person@example.com", "+1 (202) 555-0123"]),
            group(8, &["+12025550124", "+12025550123"]),
        ];
        let requested = vec!["+12025550123".to_owned(), "Person@Example.com".to_owned()];
        let matches = select_exact_groups(&values, &requested)?;
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].chat_id, 7);
        assert_eq!(
            matches[0].participants,
            vec!["+12025550123", "person@example.com"]
        );
        Ok(())
    }

    #[test]
    fn no_match_and_ambiguity_remain_explicit() -> Result<()> {
        let requested = vec!["+12025550123".to_owned(), "+12025550124".to_owned()];
        assert!(select_exact_groups(&[], &requested)?.is_empty());
        let matches = select_exact_groups(
            &[
                group(1, &["+12025550123", "+12025550124"]),
                group(2, &["+12025550124", "+12025550123"]),
            ],
            &requested,
        )?;
        assert_eq!(matches.len(), 2);
        Ok(())
    }

    #[test]
    fn group_token_and_snapshot_bind_participants_and_identity() -> Result<()> {
        let first = parse_group(&group(42, &["+12025550123", "+12025550124"]))?;
        assert!(first.token()?.starts_with("imessage-group:42:"));
        let mut changed = first.clone();
        changed.participants[1] = "+12025550125".to_owned();
        assert!(validate_group_snapshot(&first, &changed).is_err());
        changed = first.clone();
        changed.chat_guid.push_str("changed");
        assert!(validate_group_snapshot(&first, &changed).is_err());
        Ok(())
    }

    #[test]
    fn group_send_targets_existing_chat_without_recipient_or_sms_service() -> Result<()> {
        let target = parse_group(&group(42, &["+12025550123", "+12025550124"]))?;
        let item = Item {
            recipient: target.token()?,
            group: Some(target),
            text: "hello".into(),
            attachment: Some("file.mp3".into()),
            attachment_sha256: Some("hash".into()),
            source_call: Some("call".into()),
        };
        let params = send_params(&item, Some(&PathBuf::from("/tmp/file.mp3")));
        assert_eq!(params["chat_id"], 42);
        assert!(params.get("to").is_none());
        assert!(params.get("service").is_none());
        assert_eq!(params["allow_sms_fallback"], false);
        Ok(())
    }

    #[test]
    fn existing_messages_group_accepts_stored_service_metadata() -> Result<()> {
        let mut value = group(42, &["+12025550123", "person@example.invalid"]);
        value["identifier"] = json!("chat42");
        value["guid"] = json!("any;+;chat42");
        value["service"] = json!("SMS");
        let parsed = parse_group(&value)?;
        assert_eq!(parsed.chat_id, 42);
        value["is_group"] = json!(false);
        assert!(parse_group(&value).is_err());
        Ok(())
    }

    #[test]
    fn structured_delivery_dispositions_are_authoritative() {
        assert_eq!(
            classify_rpc_response(&json!({"result":{"ok":true}})),
            SendOutcome::Submitted
        );
        assert!(matches!(
            classify_rpc_response(
                &json!({"error":{"code":-32603,"message":"failed","data":{"disposition":"not_started","retry_safe":true,"detail":"permission denied before dispatch"}}})
            ),
            SendOutcome::NotStarted(_)
        ));
        assert!(matches!(
            classify_rpc_response(
                &json!({"error":{"code":-32001,"message":"unknown","data":{"disposition":"may_have_completed","retry_safe":false}}})
            ),
            SendOutcome::Uncertain(_)
        ));
        assert!(matches!(
            classify_rpc_response(
                &json!({"error":{"code":-32004,"message":"in flight","data":{"disposition":"still_in_flight","retry_safe":false}}})
            ),
            SendOutcome::Uncertain(_)
        ));
        assert!(matches!(
            classify_rpc_response(&json!({"error":{"code":-32603,"message":"unclassified"}})),
            SendOutcome::Uncertain(_)
        ));
    }
}
