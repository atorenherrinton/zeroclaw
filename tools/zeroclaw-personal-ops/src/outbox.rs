//! One prepare/review/authorize/status model over channel-specific adapters.
//! Attachments are immutable content-addressed snapshots from approved roots.
use crate::{
    Item, Ops, Plan, digest, imessage,
    journal::{Outcome, Step},
    text,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};

pub(crate) fn config(root: &Path) -> Result<Value> {
    Ok(
        toml::from_str::<toml::Value>(&std::fs::read_to_string(root.join("config.toml"))?)?
            .try_into()?,
    )
}
pub(crate) fn google_account(root: &Path) -> Result<String> {
    let c = config(root)?;
    let servers = c["mcp"]["servers"]
        .as_array()
        .context("MCP configuration missing")?;
    servers
        .iter()
        .find(|s| s["name"] == "google_write")
        .and_then(|s| s["env"]["GOG_ACCOUNT"].as_str())
        .map(str::to_owned)
        .context("Google writer account must be pinned")
}
pub(crate) async fn output(exe: &Path, args: &[String]) -> Result<std::process::Output> {
    tokio::time::timeout(
        Duration::from_secs(60),
        Command::new(exe)
            .args(args)
            .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("connector timeout; result may be uncertain")?
    .context("connector could not start")
}
pub(crate) async fn google(
    root: &Path,
    service: &str,
    method: &str,
    params: Value,
    body: Option<&Value>,
    key: &str,
) -> Result<Value> {
    ensure!(
        matches!(
            (service, method),
            (
                "gmail",
                "gmail.users.messages.send"
                    | "gmail.users.messages.list"
                    | "gmail.users.messages.get"
                    | "gmail.users.history.list"
            ) | ("calendar", "calendar.events.list")
        ),
        "unsupported Google outbox operation"
    );
    let version = if service == "gmail" { "v1" } else { "v3" };
    let scope = if method.ends_with(".send") {
        "https://www.googleapis.com/auth/gmail.send"
    } else if service == "gmail" {
        "https://www.googleapis.com/auth/gmail.readonly"
    } else {
        "https://www.googleapis.com/auth/calendar.events"
    };
    let mut args = vec![
        format!("--account={}", google_account(root)?),
        "--json".into(),
        "--no-input".into(),
        format!("--enable-commands-exact=api.call,api.{method}"),
        "api".into(),
        "call".into(),
        service.into(),
        version.into(),
        method.into(),
        format!("--params={params}"),
        format!("--scope={scope}"),
    ];
    if let Some(body) = body {
        let dir = root.join("extensions/personal-ops/requests");
        crate::private_dir(&dir)?;
        let path = dir.join(format!("{}.json", digest(key.as_bytes())));
        let bytes = serde_json::to_vec(body)?;
        if !path.exists() {
            crate::private_write(&path, &bytes)?;
        } else {
            ensure!(
                std::fs::read(&path)? == bytes,
                "request key contents changed"
            );
        }
        args.extend([
            "--allow-write".into(),
            "--force".into(),
            "--single-attempt".into(),
            format!("--body=@{}", path.display()),
        ]);
    } else {
        args.extend(["--readonly".into(), "--gmail-no-send".into()]);
    }
    // Keep existing read-only credential access on the installed read connector.
    // Only writes need the sibling's single-attempt transport extensions.
    let executable = if body.is_some() {
        root.join("extensions/google-write/gog-calendar-patch")
    } else {
        PathBuf::from("/opt/homebrew/bin/gog")
    };
    let out = output(&executable, &args).await?;
    ensure!(
        out.stdout.len() <= 4 * 1024 * 1024,
        "Google response exceeded bound"
    );
    ensure!(
        out.status.success(),
        "Google connector failed: {}",
        String::from_utf8_lossy(&out.stderr)
            .chars()
            .take(2000)
            .collect::<String>()
    );
    serde_json::from_slice(&out.stdout).context("invalid Google response")
}
async fn writer(root: &Path, name: &str, args: Value) -> Result<Value> {
    let mut child = Command::new(root.join("extensions/google-write/zeroclaw-google-write"))
        .env("GOG_ACCOUNT", google_account(root)?)
        .env("ZEROCLAW_CONFIG_DIR", root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let req = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}});
    let mut stdin = child.stdin.take().context("writer stdin missing")?;
    stdin.write_all(format!("{req}\n").as_bytes()).await?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(155), child.wait_with_output())
        .await
        .context("Calendar connector timeout")??;
    let line = out
        .stdout
        .split(|b| *b == b'\n')
        .find(|s| !s.is_empty())
        .context("writer returned no receipt")?;
    let response: Value = serde_json::from_slice(line)?;
    ensure!(
        response["result"]["isError"] != true && response.get("error").is_none(),
        "Calendar connector rejected operation: {}",
        response["result"]["content"]
    );
    response["result"]
        .get("structuredContent")
        .cloned()
        .context("Calendar receipt missing")
}
fn email_header(s: &str) -> Result<&str> {
    ensure!(
        !s.contains(['\r', '\n', '\0']),
        "email header contains control characters"
    );
    Ok(s)
}
fn staged_path(root: &Path, file: &Value) -> Result<PathBuf> {
    let name = text(file, "name", 128)?;
    ensure!(
        name.len() > 65
            && name.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
            && name.as_bytes()[64] == b'.'
            && name[65..].bytes().all(|b| b.is_ascii_alphanumeric()),
        "attachment must be an immutable staged filename"
    );
    let parent = root.join("extensions/personal-ops/files");
    let path = parent.join(name);
    let metadata = std::fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 49_000_000,
        "invalid staged attachment"
    );
    ensure!(
        path.canonicalize()?.parent() == Some(parent.canonicalize()?.as_path()),
        "attachment escapes staging"
    );
    ensure!(
        digest(&std::fs::read(&path)?) == text(file, "sha256", 64)?,
        "attachment changed since preparation"
    );
    Ok(path)
}
fn validate_communication(root: &Path, step: &Step) -> Result<()> {
    let a = &step.arguments;
    let recipients = a["recipients"]
        .as_array()
        .context("exact recipients required")?;
    ensure!(
        (1..=5).contains(&recipients.len()),
        "one to five recipients required"
    );
    let mut seen = std::collections::HashSet::new();
    for r in recipients {
        let r = r.as_str().context("recipient must be a string")?;
        ensure!(seen.insert(r.to_lowercase()), "duplicate recipient");
        ensure!(
            if step.tool == "outbox_telegram" {
                r.split(':').all(|s| s.parse::<i64>().is_ok())
            } else {
                crate::recipient(r)
            },
            "invalid exact destination"
        );
        if step.tool == "outbox_email" {
            ensure!(r.contains('@'), "email destination required");
        }
    }
    text(a, "text", 100000)?;
    if step.tool != "outbox_email" {
        ensure!(
            recipients.len() == 1,
            "each non-email step targets one recipient"
        );
    }
    if step.tool == "outbox_telegram" {
        ensure!(
            text(a, "channel_id", 128)?.starts_with("telegram."),
            "exact Telegram channel instance required"
        );
    }
    if let Some(files) = a.get("attachments") {
        let files = files.as_array().context("attachments must be array")?;
        ensure!(
            step.tool != "outbox_telegram" || files.is_empty(),
            "Telegram attachments are unsupported"
        );
        ensure!(
            step.tool != "outbox_imessage" || files.len() <= 1,
            "each iMessage step contains at most one attachment"
        );
        for file in files {
            staged_path(root, file)?;
        }
    }
    if step.tool == "outbox_email" {
        mime(a, "preflight", root)?;
    }
    Ok(())
}
fn mime(args: &Value, key: &str, root: &Path) -> Result<String> {
    let recips = args["recipients"]
        .as_array()
        .context("recipients missing")?;
    let to = recips
        .iter()
        .map(|v| email_header(v.as_str().context("recipient must be string")?))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    let subject = email_header(text(args, "subject", 998)?)?;
    let body = text(args, "text", 100000)?;
    let boundary = format!("zc{}", digest(key.as_bytes()));
    let mut mime = format!(
        "To: {to}\r\nSubject: =?UTF-8?B?{}?=\r\nMessage-ID: <{key}@outbox.zeroclaw.local>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",
        STANDARD.encode(subject),
        wrapped_base64(body.as_bytes())
    );
    let mut total = body.len();
    for file in args["attachments"].as_array().into_iter().flatten() {
        let name = text(file, "name", 128)?;
        ensure!(
            Path::new(name).file_name().is_some_and(|n| n == name),
            "invalid attachment name"
        );
        let path = staged_path(root, file)?;
        let bytes = std::fs::read(path)?;
        total += bytes.len();
        ensure!(total <= 18_000_000, "email attachments exceed 18 MB");
        ensure!(
            digest(&bytes) == text(file, "sha256", 64)?,
            "attachment changed since preparation"
        );
        mime.push_str(&format!("--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{}\r\n",wrapped_base64(&bytes)));
    }
    mime.push_str(&format!("--{boundary}--\r\n"));
    Ok(mime)
}
fn steps_for_validation(args: &Value) -> Result<Vec<Step>> {
    let steps: Vec<Step> = serde_json::from_value(args["steps"].clone())?;
    ensure!(
        (1..=20).contains(&steps.len()),
        "one to twenty transaction steps required"
    );
    Ok(steps)
}
fn wrapped_base64(bytes: &[u8]) -> String {
    STANDARD
        .encode(bytes)
        .as_bytes()
        .chunks(76)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join("\r\n")
}
impl Ops {
    pub fn outbox_prepare(&self, args: &Value) -> Result<Value> {
        let channel = text(args, "channel", 64)?;
        let recipients = args["recipients"]
            .as_array()
            .context("recipients array required")?;
        ensure!(
            (1..=5).contains(&recipients.len()),
            "one to five recipients required"
        );
        let mut seen = std::collections::HashSet::new();
        for recipient in recipients {
            let r = recipient.as_str().context("recipient must be string")?;
            ensure!(seen.insert(r.to_lowercase()), "duplicate recipient");
            ensure!(
                if channel == "telegram" {
                    r.split(':').all(|s| s.parse::<i64>().is_ok())
                } else {
                    crate::recipient(r)
                },
                "invalid exact recipient"
            );
            if channel == "email" {
                ensure!(r.contains('@'), "email destination required");
            }
        }
        text(args, "text", 100000)?;
        ensure!(
            ["email", "imessage", "telegram"].contains(&channel),
            "channel operation unsupported"
        );
        if channel == "email" {
            email_header(text(args, "subject", 998)?)?;
        }
        if channel == "telegram" {
            ensure!(
                text(args, "channel_id", 128)?.starts_with("telegram."),
                "exact configured Telegram channel_id required"
            );
        }
        let mut attachments = Vec::new();
        if args
            .get("paths")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            ensure!(
                channel != "telegram",
                "Telegram attachment outbox is unsupported; choose email/iMessage"
            );
            let prepared = self.prepare_files(
                &json!({"paths":args["paths"],"recipients":recipients,"text":args["text"]}),
            )?;
            let p: Plan = serde_json::from_value(prepared["plan"].clone())?;
            for i in p.items {
                if let (Some(name), Some(hash)) = (i.attachment, i.attachment_sha256)
                    && !attachments.iter().any(|v: &Value| v["name"] == name)
                {
                    attachments.push(json!({"name":name,"sha256":hash}));
                }
            }
        }
        let content = json!({"recipients":recipients,"text":args["text"],"subject":args.get("subject"),"attachments":attachments,"channel_id":args.get("channel_id").cloned()});
        let steps = if channel == "email" {
            vec![Step {
                tool: "outbox_email".into(),
                arguments: content,
                irreversible: true,
            }]
        } else {
            let mut steps = Vec::new();
            for recipient in recipients {
                if channel == "imessage" && !attachments.is_empty() {
                    for file in &attachments {
                        let mut args = content.clone();
                        args["recipients"] = json!([recipient]);
                        args["attachments"] = json!([file]);
                        steps.push(Step {
                            tool: "outbox_imessage".into(),
                            arguments: args,
                            irreversible: true,
                        });
                    }
                } else {
                    let mut args = content.clone();
                    args["recipients"] = json!([recipient]);
                    steps.push(Step {
                        tool: format!("outbox_{channel}"),
                        arguments: args,
                        irreversible: true,
                    });
                }
            }
            steps
        };
        let mut prepared = json!({"idempotency_key":args["idempotency_key"],"steps":steps,"title":args.get("subject").cloned().unwrap_or(json!("Message"))});
        if let Some(at) = args.get("send_at") {
            prepared["send_at"] = at.clone();
        }
        for step in &steps_for_validation(&prepared)? {
            validate_communication(&self.root, step)?;
        }
        self.operation_prepare(&prepared)
    }
    pub async fn prepare_transaction(&self, args: &Value) -> Result<Value> {
        for step in steps_for_validation(args)? {
            self.validate_step(&step).await?;
        }
        self.operation_prepare(args)
    }
    async fn validate_step(&self, step: &Step) -> Result<()> {
        if step.tool == "calendar_mutate" {
            let mut args = step.arguments.clone();
            args["idempotency_key"] = json!("preflight");
            writer(&self.root, "calendar_validate", args).await?;
        } else {
            ensure!(
                crate::journal::allowed(&step.tool),
                "unsupported transaction step"
            );
            validate_communication(&self.root, step)?;
        }
        Ok(())
    }
    pub async fn execute_operation(&self, id: &str) -> Result<Value> {
        let status = self.operation_status(id)?;
        for row in status["steps"]
            .as_array()
            .context("steps")?
            .iter()
            .filter(|r| r["state"] == "prepared")
        {
            let step: Step = serde_json::from_value(row["intent"].clone())?;
            if let Err(error) = self.validate_step(&step).await {
                let tx = self.db.unchecked_transaction()?;
                let evidence =
                    json!({"phase":"preflight","write_attempted":false,"error":error.to_string()});
                self.db.execute("UPDATE operation_steps SET state='failed',receipt=?2,updated_ms=?3 WHERE operation_id=?1 AND state='prepared'",rusqlite::params![id,evidence.to_string(),chrono::Utc::now().timestamp_millis()])?;
                self.receipt(id, None, "failed", &evidence)?;
                tx.commit()?;
                return self.operation_status(id);
            }
        }
        self.operation_execute_using(id, |step, key, reconcile| {
            self.execute_step(step, key, reconcile)
        })
        .await
    }
    async fn execute_step(&self, step: Step, key: String, reconcile: bool) -> Result<Outcome> {
        let a = &step.arguments;
        match step.tool.as_str() {
            "calendar_mutate" => {
                let mut a = a.clone();
                a["idempotency_key"] = json!(key);
                let value = if reconcile {
                    writer(
                        &self.root,
                        "calendar_reconcile",
                        json!({"idempotency_key":key}),
                    )
                    .await?
                } else {
                    writer(&self.root, "calendar_mutate", a).await?
                };
                Ok(Outcome {
                    state: value["state"].as_str().unwrap_or("uncertain").into(),
                    evidence: value,
                })
            }
            "outbox_email" => {
                // An RFC Message-ID supplies a read-only recovery anchor, never permission
                // to resend if the search result is empty (indexing can lag).
                if reconcile {
                    let v=google(&self.root,"gmail","gmail.users.messages.list",json!({"userId":"me","q":format!("in:sent rfc822msgid:{key}@outbox.zeroclaw.local"),"maxResults":2}),None,&key).await?;
                    let list = v["messages"].as_array();
                    return Ok(if let Some(list) = list.filter(|a| a.len() == 1) {
                        Outcome {
                            state: "submitted".into(),
                            evidence: json!({"provider_id":list[0]["id"],"verification":"found_in_sent","delivered":false}),
                        }
                    } else {
                        Outcome::uncertain("no unique sent message; search can lag, do not retry")
                    });
                }
                let raw = mime(a, &key, &self.root)?;
                let value = google(
                    &self.root,
                    "gmail",
                    "gmail.users.messages.send",
                    json!({"userId":"me"}),
                    Some(&json!({"raw":URL_SAFE_NO_PAD.encode(raw)})),
                    &key,
                )
                .await?;
                ensure!(value["id"].is_string(), "Gmail did not return a message ID");
                let verified = google(
                    &self.root,
                    "gmail",
                    "gmail.users.messages.get",
                    json!({"userId":"me","id":value["id"],"format":"minimal"}),
                    None,
                    &key,
                )
                .await;
                Ok(Outcome {
                    state: "submitted".into(),
                    evidence: json!({"provider_id":value["id"],"verified_in_sent":verified.as_ref().ok().and_then(|v|v["labelIds"].as_array()).is_some_and(|a|a.iter().any(|l|l=="SENT")),"delivered":false,"verification_error":verified.err().map(|e|e.to_string())}),
                })
            }
            "outbox_imessage" => {
                if reconcile {
                    return Ok(Outcome::uncertain(
                        "iMessage transport has no exact provider ID for this attempt; inspect Messages before any new send",
                    ));
                }
                let file = a["attachments"].as_array().and_then(|a| a.first());
                let item = Item {
                    recipient: a["recipients"][0].as_str().context("recipient")?.into(),
                    group: None,
                    text: text(a, "text", 100000)?.into(),
                    attachment: file.and_then(|f| f["name"].as_str()).map(str::to_owned),
                    attachment_sha256: file.and_then(|f| f["sha256"].as_str()).map(str::to_owned),
                    source_call: None,
                };
                let path: Option<PathBuf> = file.map(|f| staged_path(&self.root, f)).transpose()?;
                if let Some(path) = &path {
                    ensure!(
                        digest(&std::fs::read(path)?)
                            == item
                                .attachment_sha256
                                .as_deref()
                                .context("attachment hash")?,
                        "attachment changed"
                    );
                }
                Ok(match imessage::send_item(item, path).await {
                    imessage::SendOutcome::Submitted => Outcome {
                        state: "submitted".into(),
                        evidence: json!({"provider":"imessage","delivered":false}),
                    },
                    imessage::SendOutcome::NotStarted(e) => Outcome {
                        state: "failed".into(),
                        evidence: json!({"reason":e,"effect_started":false}),
                    },
                    imessage::SendOutcome::Uncertain(e) => Outcome::uncertain(e),
                })
            }
            "outbox_telegram" => {
                if reconcile {
                    return Ok(Outcome::uncertain(
                        "Telegram CLI has no exact message receipt; never replay an uncertain send",
                    ));
                }
                let channel = text(a, "channel_id", 64)?;
                ensure!(
                    channel.starts_with("telegram."),
                    "exact Telegram channel alias required"
                );
                let args = vec![
                    "--config-dir".into(),
                    self.root.to_string_lossy().into_owned(),
                    "channel".into(),
                    "send".into(),
                    text(a, "text", 100000)?.into(),
                    "--channel-id".into(),
                    channel.into(),
                    "--recipient".into(),
                    a["recipients"][0].as_str().context("recipient")?.into(),
                ];
                let exe = PathBuf::from(std::env::var_os("HOME").context("HOME")?)
                    .join(".cargo/bin/zeroclaw");
                let out = output(&exe, &args).await?;
                if !out.status.success() {
                    bail!("Telegram send outcome uncertain");
                }
                Ok(Outcome {
                    state: "submitted".into(),
                    evidence: json!({"provider":"telegram","via":"zeroclaw channel send","delivered":false}),
                })
            }
            _ => bail!("unsupported operation"),
        }
    }
    pub async fn dispatch_operations(&self) -> Result<Value> {
        let ids=self.db.prepare("SELECT id FROM operations WHERE authorized_ms IS NOT NULL AND cancelled=0 AND COALESCE(send_at_ms,authorized_ms)<=?1 AND EXISTS(SELECT 1 FROM operation_steps WHERE operation_id=operations.id AND state='prepared') AND NOT EXISTS(SELECT 1 FROM operation_steps WHERE operation_id=operations.id AND state IN ('failed','uncertain')) ORDER BY COALESCE(send_at_ms,authorized_ms) LIMIT 20")?.query_map([chrono::Utc::now().timestamp_millis()],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut results = Vec::new();
        for id in ids {
            let result = self.execute_operation(&id).await;
            results.push(json!({"operation_id":id,"state":result.as_ref().ok().map(|v|&v["state"]),"error":result.err().map(|e|e.to_string())}));
        }
        Ok(json!({"operations":results}))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mime_injection_and_attachment_tampering_fail() -> Result<()> {
        let t = tempfile::tempdir()?;
        let a = json!({"recipients":["a@example.invalid"],"subject":"Fixture\r\nBcc: x@example.invalid","text":"hello","attachments":[]});
        assert!(mime(&a, "fixture", t.path()).is_err());
        let mut a = a;
        a["subject"] = json!("Fixture");
        let raw = mime(&a, "fixture", t.path())?;
        assert!(raw.contains("Message-ID: <fixture@outbox.zeroclaw.local>"));
        assert!(raw.contains("aGVsbG8="));
        Ok(())
    }
    #[tokio::test]
    async fn raw_transaction_rejects_attachment_escape_atomically() -> Result<()> {
        let t = tempfile::tempdir()?;
        let ops = Ops::open(t.path())?;
        let plan = json!({"idempotency_key":"fixture-path","steps":[{"tool":"outbox_email","arguments":{"recipients":["owner@example.invalid"],"subject":"Fixture","text":"hello","attachments":[]}},{"tool":"outbox_imessage","arguments":{"recipients":["owner@example.invalid"],"text":"hello","attachments":[{"name":"../../config.toml","sha256":"fixture"}]}}]});
        assert!(ops.prepare_transaction(&plan).await.is_err());
        assert_eq!(
            ops.db
                .query_row("SELECT count(*) FROM operations", [], |r| r
                    .get::<_, i64>(0))?,
            0
        );
        // Old/corrupt persisted plans are preflighted as a whole before dispatch.
        let prepared = ops.operation_prepare(&plan)?;
        ops.operation_authorize(&json!({"operation_id":"fixture-path","owner_requested_send":true,"review_hash":prepared["review_hash"],"review":prepared["review"]}))?;
        let result = ops.execute_operation("fixture-path").await?;
        assert_eq!(result["state"], "failed");
        assert!(
            result["steps"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s["receipt"]["write_attempted"] == false)
        );
        Ok(())
    }
}
