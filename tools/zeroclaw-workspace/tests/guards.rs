//! Synthetic read/verification boundary tests; no Google or credential access.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use zeroclaw_workspace::{
    api::{Api, read_target},
    model::Read,
    operations::{self, tab_text, text_hash},
};
struct Fake {
    value: Value,
    reads: usize,
}
impl Api for Fake {
    async fn read(&mut self, _: &Read) -> Result<Value> {
        self.reads += 1;
        Ok(self.value.clone())
    }
}
fn doc(text: &str) -> Value {
    json!({"documentId":"doc1","revisionId":"r1","suggestionsViewMode":"SUGGESTIONS_INLINE","tabs":[{"tabProperties":{"tabId":"t.0"},"documentTab":{"body":{"content":[{"sectionBreak":{}},{"paragraph":{"elements":[{"textRun":{"content":text}}]}}]}}}]})
}
fn verify(text: &str) -> Read {
    Read::DocsVerify {
        document_id: "doc1".into(),
        tab_id: "t.0".into(),
        expected_text_sha256: text_hash(text),
        expected_revision_id: Some("r1".into()),
    }
}
#[test]
fn exact_get_targets_and_inline_all_tab_query() -> Result<()> {
    for request in [
        Read::DocsRead {
            document_id: "doc1".into(),
        },
        verify("a\n"),
    ] {
        let (url, q) = read_target(&request)?;
        assert_eq!(url, "https://docs.googleapis.com/v1/documents/doc1");
        assert!(q.contains(&("includeTabsContent", "true".into())));
        assert!(q.contains(&("suggestionsViewMode", "SUGGESTIONS_INLINE".into())));
    }
    for id in [
        "../permissions",
        "https://evil.invalid",
        "x?alt=media",
        "x%2fpermissions",
        "x:batchUpdate",
        "",
    ] {
        assert!(
            read_target(&Read::DocsRead {
                document_id: id.into()
            })
            .is_err()
        );
    }
    Ok(())
}
#[test]
fn discovery_escapes_literals_and_is_bounded() -> Result<()> {
    let (url, q) = read_target(&Read::DriveList {
        name_contains: Some("a'\\b".into()),
        page_token: None,
    })?;
    assert_eq!(url, "https://www.googleapis.com/drive/v3/files");
    assert!(q.contains(&("pageSize", "20".into())));
    assert!(q[0].1.ends_with("name contains 'a\\'\\\\b'"));
    assert!(
        Read::DriveList {
            name_contains: Some("x\n".into()),
            page_token: None
        }
        .validate()
        .is_err()
    );
    assert!(
        Read::DriveList {
            name_contains: None,
            page_token: Some("x".repeat(2049))
        }
        .validate()
        .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn unicode_text_matches_only_exact_bytes_and_revision() -> Result<()> {
    let mut api = Fake {
        value: doc("Parts 🛠\n"),
        reads: 0,
    };
    let result = operations::read(&mut api, &verify("Parts 🛠\n")).await?;
    assert_eq!(result["matches"], true);
    assert_eq!(result["read_only"], true);
    assert_eq!(result["text_bytes"], 11);
    assert_eq!(
        operations::read(&mut api, &verify("Parts 🛠")).await?["matches"],
        false
    );
    api.value["revisionId"] = json!("r2");
    assert!(
        operations::read(&mut api, &verify("Parts 🛠\n"))
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn missing_revision_and_wrong_resource_fail_closed() -> Result<()> {
    let mut api = Fake {
        value: doc("\n"),
        reads: 0,
    };
    api.value.as_object_mut().unwrap().remove("revisionId");
    assert!(operations::read(&mut api, &verify("\n")).await.is_err());
    api.value["documentId"] = json!("other");
    assert!(
        operations::read(
            &mut api,
            &Read::DocsRead {
                document_id: "doc1".into()
            }
        )
        .await
        .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn missing_inline_evidence_is_not_a_successful_verification() {
    let mut api = Fake {
        value: doc("\n"),
        reads: 0,
    };
    api.value["suggestionsViewMode"] = json!("PREVIEW_WITHOUT_SUGGESTIONS");
    assert!(operations::read(&mut api, &verify("\n")).await.is_err());
}
#[test]
fn child_tabs_are_exact_and_duplicate_ids_are_denied() -> Result<()> {
    let mut d = doc("parent\n");
    let mut child = doc("child\n")["tabs"][0].clone();
    child["tabProperties"]["tabId"] = json!("t.child");
    d["tabs"][0]["childTabs"] = json!([child]);
    assert_eq!(tab_text(&d, "t.child")?, "child\n");
    assert!(tab_text(&d, "missing").is_err());
    d["tabs"][0]["childTabs"][0]["tabProperties"]["tabId"] = json!("t.0");
    assert!(tab_text(&d, "t.0").is_err());
    Ok(())
}
#[test]
fn complex_content_and_suggestions_never_match_partial_text() {
    for field in [
        "headers",
        "footers",
        "footnotes",
        "inlineObjects",
        "positionedObjects",
    ] {
        let mut d = doc("\n");
        d["tabs"][0]["documentTab"][field] = json!({"x":{}});
        assert!(tab_text(&d, "t.0").is_err());
    }
    for extra in [
        json!({"table":{}}),
        json!({"tableOfContents":{}}),
        json!({"paragraph":{"elements":[{"inlineObjectElement":{}}]}}),
        json!({"paragraph":{"elements":[{"textRun":{"content":"\n"},"suggestedInsertionIds":["s1"]}]}}),
    ] {
        let mut d = doc("\n");
        d["tabs"][0]["documentTab"]["body"]["content"]
            .as_array_mut()
            .unwrap()
            .push(extra);
        assert!(tab_text(&d, "t.0").is_err());
    }
}
#[test]
fn text_and_tab_tree_bounds_reject_without_truncation() {
    assert!(tab_text(&doc(&"x".repeat(100001)), "t.0").is_err());
    assert!(tab_text(&doc("no terminator"), "t.0").is_err());
    let mut d = doc("\n");
    d["tabs"] = json!(
        (0..101)
            .map(|i| json!({"tabProperties":{"tabId":format!("t.{i}")}}))
            .collect::<Vec<_>>()
    );
    assert!(tab_text(&d, "t.0").is_err());
}
#[tokio::test]
async fn invalid_reads_never_reach_api() {
    let mut api = Fake {
        value: json!({}),
        reads: 0,
    };
    assert!(
        operations::read(
            &mut api,
            &Read::DocsRead {
                document_id: "../bad".into()
            }
        )
        .await
        .is_err()
    );
    assert_eq!(api.reads, 0);
}
#[tokio::test]
async fn provider_errors_propagate_without_retry() {
    struct Failing(usize);
    impl Api for Failing {
        async fn read(&mut self, _: &Read) -> Result<Value> {
            self.0 += 1;
            bail!("fixture unavailable")
        }
    }
    let mut api = Failing(0);
    assert!(operations::read(&mut api, &verify("\n")).await.is_err());
    assert_eq!(api.0, 1);
}
#[test]
fn write_sheets_and_extra_authority_fields_are_not_in_vocabulary() {
    for args in [
        json!({"action":"docs_create","title":"Synthetic"}),
        json!({"action":"sheets_read","spreadsheet_id":"s1","range":"'Sheet1'!A1:A1"}),
        json!({"action":"docs_read","document_id":"doc1","approved":true}),
        json!({"action":"docs_read","document_id":"doc1","owner_requested":true}),
    ] {
        assert!(serde_json::from_value::<Read>(args).is_err());
    }
}

#[test]
fn mixed_structure_cannot_hide_behind_valid_text() {
    let mut d = doc("body\n");
    d["tabs"][0]["documentTab"]["body"]["content"][1]["table"] = json!({});
    assert!(tab_text(&d, "t.0").is_err());
    let mut d = doc("body\n");
    d["tabs"][0]["documentTab"]["body"]["content"][1]["paragraph"]["elements"][0]["inlineObjectElement"] =
        json!({});
    assert!(tab_text(&d, "t.0").is_err());
}
