use crate::{
    api::Api,
    model::{MAX_TEXT, Read},
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn text_hash(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// Exact body-text projection only. It is not a rendering or formatting proof.
pub fn tab_text(document: &Value, tab_id: &str) -> Result<String> {
    let tabs = document["tabs"]
        .as_array()
        .context("tab-aware document required")?;
    let mut pending: Vec<_> = tabs.iter().map(|tab| (tab, 0usize)).collect();
    let mut found = None;
    let mut seen = std::collections::HashSet::new();
    while let Some((tab, depth)) = pending.pop() {
        ensure!(depth <= 16 && seen.len() < 100, "tab tree exceeds bound");
        let id = tab["tabProperties"]["tabId"]
            .as_str()
            .context("tab ID missing")?;
        ensure!(seen.insert(id), "duplicate tab ID");
        if id == tab_id {
            found = Some(tab);
        }
        if let Some(children) = tab.get("childTabs") {
            let children = children.as_array().context("invalid child tabs")?;
            ensure!(
                pending.len() + children.len() <= 100,
                "tab tree exceeds bound"
            );
            pending.extend(children.iter().map(|child| (child, depth + 1)));
        }
    }
    let tab = &found.context("exact tab not found")?["documentTab"];
    for field in [
        "headers",
        "footers",
        "footnotes",
        "inlineObjects",
        "positionedObjects",
        "lists",
    ] {
        ensure!(
            tab.get(field)
                .is_none_or(|v| v.as_object().is_some_and(|m| m.is_empty())),
            "rich tab structures cannot be verified as plain text"
        );
    }
    let content = tab["body"]["content"]
        .as_array()
        .context("tab body missing")?;
    let mut text = String::new();
    for element in content {
        if element.get("sectionBreak").is_some() {
            ensure!(
                element.as_object().is_some_and(|m| m.keys().all(|k| [
                    "startIndex",
                    "endIndex",
                    "sectionBreak"
                ]
                .contains(&k.as_str()))),
                "ambiguous section break"
            );
            continue;
        }
        ensure!(
            element.as_object().is_some_and(|m| m.keys().all(|k| [
                "startIndex",
                "endIndex",
                "paragraph"
            ]
            .contains(&k.as_str()))),
            "unsupported body structure"
        );
        let paragraph = element
            .get("paragraph")
            .context("only plain paragraphs can be verified")?;
        ensure!(
            paragraph.get("positionedObjectIds").is_none(),
            "positioned objects cannot be verified as plain text"
        );
        ensure!(
            paragraph.as_object().is_some_and(|m| m
                .keys()
                .all(|k| ["elements", "paragraphStyle"].contains(&k.as_str()))),
            "unsupported paragraph structure"
        );
        let runs = paragraph["elements"]
            .as_array()
            .context("paragraph elements missing")?;
        for run in runs {
            ensure!(
                run.as_object().is_some_and(|m| m.keys().all(|k| [
                    "startIndex",
                    "endIndex",
                    "textRun",
                    "suggestedInsertionIds",
                    "suggestedDeletionIds"
                ]
                .contains(&k.as_str()))),
                "unsupported paragraph element"
            );
            ensure!(
                run.get("suggestedInsertionIds").is_none()
                    && run.get("suggestedDeletionIds").is_none(),
                "suggested text cannot be verified"
            );
            let value = run["textRun"]["content"]
                .as_str()
                .context("only plain text runs can be verified")?;
            ensure!(
                text.len() + value.len() <= MAX_TEXT,
                "tab text exceeds verification bound"
            );
            text.push_str(value);
        }
    }
    ensure!(text.ends_with('\n'), "document body terminator missing");
    Ok(text)
}

pub async fn read(api: &mut impl Api, request: &Read) -> Result<Value> {
    request.validate()?;
    let value = api.read(request).await?;
    match request {
        Read::DocsRead { document_id } | Read::DocsVerify { document_id, .. } => {
            ensure!(
                value["documentId"] == *document_id,
                "document identity mismatch"
            );
        }
        Read::DriveMetadata { file_id } => {
            ensure!(value["id"] == *file_id, "file identity mismatch")
        }
        Read::DriveList { .. } => {}
    }
    if let Read::DocsVerify {
        document_id,
        tab_id,
        expected_text_sha256,
        expected_revision_id,
    } = request
    {
        if let Some(revision) = expected_revision_id {
            ensure!(
                value["revisionId"] == *revision,
                "document revision changed or unavailable"
            );
        }
        ensure!(
            value["suggestionsViewMode"] == "SUGGESTIONS_INLINE",
            "inline suggestions evidence missing"
        );
        let text = tab_text(&value, tab_id)?;
        let actual = text_hash(&text);
        return Ok(
            json!({"untrusted_content":true,"document_id":document_id,"tab_id":tab_id,"revision_id":value["revisionId"],"text_sha256":actual,"text_bytes":text.len(),"matches":actual == *expected_text_sha256,"verification_scope":"exact plain body text in the selected tab, including final newline; excludes formatting and other tabs","read_only":true}),
        );
    }
    Ok(json!({"untrusted_content":true,"data":value,"read_only":true}))
}
