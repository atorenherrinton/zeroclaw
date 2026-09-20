//! One terminal-approved create/populate intent. Uncertain dispatches never replay.
use crate::{
    api::{Api, Workspace},
    model::{MAX_TEXT, Read, id},
    operations::{tab_text, text_hash},
    state::State,
};
use anyhow::{Context, Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::{Read as IoRead, Write},
    path::Path,
};
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Apply {
    pub operation_id: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub operation_id: String,
    pub title: String,
    pub text: String,
}
impl Intent {
    pub fn validate(&self) -> Result<()> {
        id(&self.operation_id)?;
        ensure!(self.operation_id.len() <= 100, "operation ID too long");
        ensure!(
            !self.title.is_empty()
                && self.title.len() <= 200
                && !self.title.chars().any(char::is_control),
            "invalid document title"
        );
        ensure!(
            !self.text.is_empty()
                && self.text.len() < MAX_TEXT
                && self
                    .text
                    .chars()
                    .all(|c| !c.is_control() || c == '\n' || c == '\t'),
            "invalid plain document content"
        );
        Ok(())
    }
    fn marker(&self, account: &str, client: &str) -> Result<String> {
        Ok(text_hash(&serde_json::to_string(&(
            self,
            account.to_ascii_lowercase(),
            client,
        ))?))
    }
}
#[derive(Serialize, Deserialize, PartialEq, Debug)]
enum Phase {
    Granted,
    Creating,
    Created,
    Populating,
    Verified,
}
#[derive(Serialize, Deserialize)]
struct Record {
    intent: Intent,
    account: String,
    client: String,
    marker: String,
    phase: Phase,
    document_id: Option<String>,
}
fn key(operation: &str) -> Result<String> {
    id(operation)?;
    ensure!(operation.len() <= 100, "operation ID too long");
    Ok(format!("grant-{operation}"))
}
pub trait WriteApi: Api {
    fn create(&mut self, title: &str, marker: &str) -> impl Future<Output = Result<Value>>;
    fn find(&mut self, marker: &str) -> impl Future<Output = Result<Value>>;
    fn populate(
        &mut self,
        document: &str,
        tab: &str,
        revision: &str,
        text: &str,
    ) -> impl Future<Output = Result<Value>>;
}
pub fn create_body(title: &str, marker: &str) -> Result<Value> {
    id(marker)?;
    ensure!(
        !title.is_empty() && title.len() <= 200 && !title.chars().any(char::is_control),
        "invalid title"
    );
    Ok(
        json!({"name":title,"mimeType":"application/vnd.google-apps.document","appProperties":{"zeroclaw_intent":marker}}),
    )
}
pub fn populate_body(document: &str, tab: &str, revision: &str, text: &str) -> Result<Value> {
    id(document)?;
    Read::DocsVerify {
        document_id: document.into(),
        tab_id: tab.into(),
        expected_text_sha256: text_hash(text),
        expected_revision_id: Some(revision.into()),
    }
    .validate()?;
    ensure!(
        !text.is_empty() && text.len() < MAX_TEXT,
        "invalid text size"
    );
    Ok(
        json!({"writeControl":{"requiredRevisionId":revision},"requests":[{"insertText":{"location":{"index":1,"tabId":tab},"text":text}}]}),
    )
}
impl WriteApi for Workspace {
    async fn create(&mut self, title: &str, marker: &str) -> Result<Value> {
        self.mutation(
            "https://www.googleapis.com/drive/v3/files?fields=id",
            &create_body(title, marker)?,
        )
        .await
    }
    async fn find(&mut self, marker: &str) -> Result<Value> {
        self.lookup(marker).await
    }
    async fn populate(
        &mut self,
        document: &str,
        tab: &str,
        revision: &str,
        text: &str,
    ) -> Result<Value> {
        let body = populate_body(document, tab, revision, text)?;
        self.mutation(
            &format!("https://docs.googleapis.com/v1/documents/{document}:batchUpdate"),
            &body,
        )
        .await
    }
}
#[derive(Serialize, Deserialize)]
struct SealedRecord {
    record: Record,
    mac: Vec<u8>,
}
fn save_record(state: &State, key: &str, record: &Record, secret: &[u8]) -> Result<()> {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret)
        .map_err(|_| anyhow::Error::msg("invalid journal key"))?;
    mac.update(&serde_json::to_vec(record)?);
    state.save(
        key,
        &json!({"record":record,"mac":mac.finalize().into_bytes().to_vec()}),
    )
}
fn load_record(state: &State, key: &str, secret: &[u8]) -> Result<Option<Record>> {
    use hmac::Mac;
    let Some(sealed) = state.read::<SealedRecord>(key)? else {
        return Ok(None);
    };
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret)
        .map_err(|_| anyhow::Error::msg("invalid journal key"))?;
    mac.update(&serde_json::to_vec(&sealed.record)?);
    mac.verify_slice(&sealed.mac)
        .map_err(|_| anyhow::Error::msg("owner journal authentication failed"))?;
    Ok(Some(sealed.record))
}
fn grant(state: &State, intent: Intent, account: &str, client: &str, secret: &[u8]) -> Result<()> {
    intent.validate()?;
    let key = key(&intent.operation_id)?;
    ensure!(
        state.read::<Value>(&key)?.is_none(),
        "operation already granted; use the original operation without reauthorizing"
    );
    let marker = intent.marker(account, client)?;
    save_record(
        state,
        &key,
        &Record {
            intent,
            account: account.to_ascii_lowercase(),
            client: client.into(),
            marker,
            phase: Phase::Granted,
            document_id: None,
        },
        secret,
    )
}
/// This issuer is CLI-only. Model tools cannot supply files, text or approval flags.
pub async fn authorize(operation: &str, title: &str, path: &Path) -> Result<()> {
    crate::consent::terminal()?;
    key(operation)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_TEXT as u64)
        .read_to_end(&mut bytes)?;
    let text = String::from_utf8(bytes).context("plain UTF-8 content required")?;
    let intent = Intent {
        operation_id: operation.into(),
        title: title.into(),
        text,
    };
    intent.validate()?;
    let api = Workspace::connect(false).await?;
    let digest = intent.marker(&api.account, &api.client_id)?;
    // JSON escaping prevents terminal controls in the account/title/content preview.
    eprintln!(
        "One new Google Doc; no sharing. Exact UTF-8 file bytes are inserted as plain text; Google adds its final newline.\nReview content: {}\nAccount: {}\nTitle: {}\nOperation: {}\nIntent SHA256: {}\nType the full intent SHA256 to authorize this one operation:",
        serde_json::to_string(&intent.text)?,
        serde_json::to_string(&api.account)?,
        serde_json::to_string(title)?,
        operation,
        digest
    );
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        answer.trim() == digest,
        "owner cancelled document authorization"
    );
    crate::owner::confirm(&format!(
        "Authorize one new Google Doc: {}. Intent SHA256 {}. No sharing.",
        title, digest
    ))?;
    let state = State::open(&zeroclaw_gmail::auth::root()?)?;
    let secret = crate::owner::journal_key(true)?;
    grant(&state, intent, &api.account, &api.client_id, &secret)?;
    println!(
        "{}",
        json!({"operation_id":operation,"authorized":true,"intent_sha256":digest})
    );
    Ok(())
}
fn no_suggestions(value: &Value) -> Result<()> {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                ensure!(
                    !key.starts_with("suggested"),
                    "suggested structures cannot be verified"
                );
                no_suggestions(value)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                no_suggestions(item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Reject omitted tabs and rich structures before claiming whole-document text proof.
fn plain(document: &Value, document_id: &str) -> Result<(String, String, String)> {
    no_suggestions(document)?;
    ensure!(
        document["documentId"] == document_id
            && document["suggestionsViewMode"] == "SUGGESTIONS_INLINE",
        "document identity/suggestions mismatch"
    );
    let tabs = document["tabs"]
        .as_array()
        .context("all-tab response required")?;
    ensure!(
        tabs.len() == 1
            && tabs[0]
                .get("childTabs")
                .is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)),
        "one plain tab required"
    );
    // Reject legacy top-level structures as well as any nested tab structures.
    for name in [
        "headers",
        "footers",
        "footnotes",
        "inlineObjects",
        "positionedObjects",
        "body",
    ] {
        ensure!(
            document.get(name).is_none(),
            "ambiguous legacy/rich structures"
        );
    }
    let tab = tabs[0]["tabProperties"]["tabId"]
        .as_str()
        .context("tab ID missing")?;
    let revision = document["revisionId"]
        .as_str()
        .context("revision required")?;
    let text = tab_text(document, tab)?;
    Read::DocsVerify {
        document_id: document_id.into(),
        tab_id: tab.into(),
        expected_text_sha256: text_hash(&text),
        expected_revision_id: Some(revision.into()),
    }
    .validate()?;
    Ok((tab.into(), revision.into(), text))
}
pub async fn apply(
    api: &mut impl WriteApi,
    state: &State,
    operation: &str,
    account: &str,
    client: &str,
    secret: &[u8],
) -> Result<Value> {
    let key = key(operation)?;
    let mut r: Record =
        load_record(state, &key, secret)?.context("owner terminal grant required")?;
    r.intent.validate()?;
    ensure!(
        r.intent.operation_id == operation
            && r.account.eq_ignore_ascii_case(account)
            && r.client == client
            && r.marker == r.intent.marker(account, client)?,
        "owner grant intent/account/client mismatch"
    );
    if r.phase == Phase::Granted {
        r.phase = Phase::Creating;
        save_record(state, &key, &r, secret)?; // durable BEFORE dispatch
        let reply = api.create(&r.intent.title, &r.marker).await?;
        let document = reply["id"]
            .as_str()
            .context("create response uncertain; reconcile")?;
        id(document)?;
        r.document_id = Some(document.into());
        r.phase = Phase::Created;
        save_record(state, &key, &r, secret)?;
    }
    if r.phase == Phase::Creating {
        let found = api.find(&r.marker).await?;
        ensure!(
            found.get("nextPageToken").is_none(),
            "ambiguous create reconciliation"
        );
        let files = found["files"]
            .as_array()
            .context("invalid reconciliation response")?;
        ensure!(
            files.len() == 1,
            "create uncertain: no unique app-owned document; never retry create"
        );
        let file = &files[0];
        ensure!(
            file["appProperties"]["zeroclaw_intent"] == r.marker
                && file["mimeType"] == "application/vnd.google-apps.document"
                && file["name"] == r.intent.title
                && file["ownedByMe"] == true
                && file["trashed"] == false,
            "create reconciliation mismatch"
        );
        let document = file["id"].as_str().context("document ID missing")?;
        id(document)?;
        r.document_id = Some(document.into());
        r.phase = Phase::Created;
        save_record(state, &key, &r, secret)?;
    }
    let document = r
        .document_id
        .clone()
        .context("journal document identity missing")?;
    id(&document)?;
    let value = api
        .read(&Read::DocsRead {
            document_id: document.clone(),
        })
        .await?;
    let (tab, revision, text) = plain(&value, &document)?;
    let expected = format!("{}\n", r.intent.text);
    if text != expected {
        ensure!(
            r.phase == Phase::Created && text == "\n",
            "document changed or population uncertain; never overwrite or replay"
        );
        r.phase = Phase::Populating;
        save_record(state, &key, &r, secret)?; // durable BEFORE dispatch
        api.populate(&document, &tab, &revision, &r.intent.text)
            .await?;
        let observed = api
            .read(&Read::DocsRead {
                document_id: document.clone(),
            })
            .await?;
        let (_, _, actual) = plain(&observed, &document)?;
        ensure!(
            actual == expected,
            "post-write readback mismatch; reconcile same operation"
        );
    }
    r.phase = Phase::Verified;
    save_record(state, &key, &r, secret)?;
    Ok(
        json!({"document_id":document,"url":format!("https://docs.google.com/document/d/{document}/edit"),"verified":true,"text_sha256":text_hash(&expected),"verification_scope":"entire single-tab plain body including final newline; formatting not verified","untrusted_content":true}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        creates: usize,
        populates: usize,
        text: String,
        lose_create: bool,
        lose_populate: bool,
        found: usize,
        commit_insert: bool,
    }
    fn doc(text: &str) -> Value {
        json!({"documentId":"doc1","revisionId":"r1","suggestionsViewMode":"SUGGESTIONS_INLINE","tabs":[{"tabProperties":{"tabId":"t.0"},"documentTab":{"body":{"content":[{"paragraph":{"elements":[{"textRun":{"content":text}}]}}]}}}]})
    }
    impl Api for Fake {
        async fn read(&mut self, _: &Read) -> Result<Value> {
            Ok(doc(&self.text))
        }
    }
    impl WriteApi for Fake {
        async fn create(&mut self, _: &str, _: &str) -> Result<Value> {
            self.creates += 1;
            ensure!(!self.lose_create, "lost response");
            Ok(json!({"id":"doc1"}))
        }
        async fn find(&mut self, m: &str) -> Result<Value> {
            Ok(
                json!({"files":vec![json!({"id":"doc1","name":"Fixture","mimeType":"application/vnd.google-apps.document","ownedByMe":true,"trashed":false,"appProperties":{"zeroclaw_intent":m}});self.found]}),
            )
        }
        async fn populate(&mut self, _: &str, _: &str, r: &str, t: &str) -> Result<Value> {
            assert_eq!(r, "r1");
            self.populates += 1;
            if self.commit_insert {
                self.text = format!("{t}\n");
            }
            ensure!(!self.lose_populate, "lost response");
            Ok(json!({}))
        }
    }
    #[tokio::test]
    async fn uncertain_create_and_populate_reconcile_without_duplicates() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = State::open(&root.path().canonicalize()?)?;
        let intent = Intent {
            operation_id: "op1".into(),
            title: "Fixture".into(),
            text: "Parts 🛠\n".into(),
        };
        grant(&state, intent, "owner@example.invalid", "client", &[7; 32])?;
        let mut api = Fake {
            creates: 0,
            populates: 0,
            text: "\n".into(),
            lose_create: true,
            lose_populate: true,
            found: 1,
            commit_insert: true,
        };
        assert!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert_eq!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await?["verified"],
            true
        );
        assert_eq!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await?["verified"],
            true
        );
        assert_eq!((api.creates, api.populates), (1, 1));
        api.text = "changed\n".into();
        assert!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert!(
            apply(
                &mut api,
                &state,
                "op1",
                "other@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert!(
            apply(
                &mut api,
                &state,
                "op1",
                "owner@example.invalid",
                "other",
                &[7; 32]
            )
            .await
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn strict_body_and_all_tab_proof() -> Result<()> {
        let b = populate_body("doc1", "t.0", "r1", "text")?;
        assert_eq!(b["writeControl"]["requiredRevisionId"], "r1");
        assert_eq!(b["requests"].as_array().unwrap().len(), 1);
        assert!(populate_body("../permissions", "t.0", "r1", "text").is_err());
        let mut d = doc("\n");
        let extra = d["tabs"][0].clone();
        d["tabs"].as_array_mut().unwrap().push(extra);
        assert!(plain(&d, "doc1").is_err());
        Ok(())
    }
    #[test]
    fn journal_lock_grant_replay_and_private_permissions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = State::open(&root.path().canonicalize()?)?;
        assert!(State::open(&root.path().canonicalize()?).is_err());
        let intent = Intent {
            operation_id: "op1".into(),
            title: "Fixture".into(),
            text: "text".into(),
        };
        grant(
            &state,
            intent.clone(),
            "owner@example.invalid",
            "client",
            &[7; 32],
        )?;
        assert!(grant(&state, intent, "owner@example.invalid", "client", &[7; 32]).is_err());
        drop(state);
        assert!(State::open(&root.path().canonicalize()?).is_ok());
        Ok(())
    }
    #[tokio::test]
    async fn forged_or_altered_journal_never_dispatches() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = State::open(&root.path().canonicalize()?)?;
        let intent = Intent {
            operation_id: "fixture".into(),
            title: "Fixture".into(),
            text: "intended".into(),
        };
        grant(&state, intent, "owner@example.invalid", "client", &[7; 32])?;
        let mut record: Value = state.read("grant-fixture")?.unwrap();
        record["record"]["intent"]["text"] = json!("injected");
        state.save("grant-fixture", &record)?;
        let mut api = Fake {
            creates: 0,
            populates: 0,
            text: "\n".into(),
            lose_create: false,
            lose_populate: false,
            found: 1,
            commit_insert: true,
        };
        assert!(
            apply(
                &mut api,
                &state,
                "fixture",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert_eq!((api.creates, api.populates), (0, 0));
        Ok(())
    }
    #[tokio::test]
    async fn nonblank_document_is_never_overwritten() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = State::open(&root.path().canonicalize()?)?;
        grant(
            &state,
            Intent {
                operation_id: "fixture".into(),
                title: "Fixture".into(),
                text: "intended".into(),
            },
            "owner@example.invalid",
            "client",
            &[7; 32],
        )?;
        let mut api = Fake {
            creates: 0,
            populates: 0,
            text: "other content\n".into(),
            lose_create: false,
            lose_populate: false,
            found: 1,
            commit_insert: true,
        };
        assert!(
            apply(
                &mut api,
                &state,
                "fixture",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert_eq!(api.populates, 0);
        Ok(())
    }
    #[tokio::test]
    async fn missing_or_ambiguous_create_and_uncommitted_insert_never_retry() -> Result<()> {
        let root = tempfile::tempdir()?;
        let state = State::open(&root.path().canonicalize()?)?;
        grant(
            &state,
            Intent {
                operation_id: "fixture".into(),
                title: "Fixture".into(),
                text: "intended".into(),
            },
            "owner@example.invalid",
            "client",
            &[7; 32],
        )?;
        let mut api = Fake {
            creates: 0,
            populates: 0,
            text: "\n".into(),
            lose_create: true,
            lose_populate: true,
            found: 0,
            commit_insert: false,
        };
        for found in [0, 0, 2] {
            api.found = found;
            assert!(
                apply(
                    &mut api,
                    &state,
                    "fixture",
                    "owner@example.invalid",
                    "client",
                    &[7; 32]
                )
                .await
                .is_err()
            );
        }
        assert_eq!((api.creates, api.populates), (1, 0));
        api.found = 1;
        assert!(
            apply(
                &mut api,
                &state,
                "fixture",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert!(
            apply(
                &mut api,
                &state,
                "fixture",
                "owner@example.invalid",
                "client",
                &[7; 32]
            )
            .await
            .is_err()
        );
        assert_eq!((api.creates, api.populates), (1, 1));
        Ok(())
    }
}
