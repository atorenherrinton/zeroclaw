use crate::plan::PlanEntry;

/// Structured metadata for a tool that produced a file artifact (e.g.
/// `deliver_file`). Carried on [`TurnEvent::ToolResult`] so a channel attaches
/// the file from typed fields instead of parsing a text trailer out of the
/// free-form `output` string. Trailer parsing let a crafted filename forge the
/// delivered path (arbitrary-file-read / confused-deputy class).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolArtifact {
    /// Absolute path of the delivered file on the agent host.
    pub path: String,
    /// Stable citation URI the client can reference (e.g. `attachment://…`).
    pub uri: String,
    /// Original filename.
    pub filename: String,
    /// Human-readable chat label; defaults to the filename.
    pub title: String,
    /// MIME type.
    pub mime: String,
    /// Size in bytes.
    pub size: u64,
}

/// One borrowed serialization shape for admission and the owned artifact.
/// This view resolves fields from the source; it does not copy their payloads.
#[derive(serde::Serialize)]
struct ArtifactFields<'a> {
    path: &'a str,
    uri: &'a str,
    filename: &'a str,
    title: &'a str,
    mime: &'a str,
    size: u64,
}

impl<'a> ArtifactFields<'a> {
    fn from_delivered_data(data: &'a serde_json::Value) -> Option<Self> {
        if data.get("delivered").and_then(serde_json::Value::as_bool) != Some(true) {
            return None;
        }
        let field = |key: &str| {
            data.get(key)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        };
        let path = field("path");
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path,
            uri: field("uri"),
            filename: field("filename"),
            title: field("title"),
            mime: field("mimeType"),
            size: data
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        })
    }

    fn into_owned(self) -> ToolArtifact {
        ToolArtifact {
            path: self.path.to_owned(),
            uri: self.uri.to_owned(),
            filename: self.filename.to_owned(),
            title: self.title.to_owned(),
            mime: self.mime.to_owned(),
            size: self.size,
        }
    }
}

impl serde::Serialize for ToolArtifact {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ArtifactFields {
            path: &self.path,
            uri: &self.uri,
            filename: &self.filename,
            title: &self.title,
            mime: &self.mime,
            size: self.size,
        }
        .serialize(serializer)
    }
}

impl ToolArtifact {
    /// Build from delivered structured data, preserving the existing uncapped
    /// conversion API. Runtime emitters use the bounded conversion below.
    pub fn from_delivered_data(data: &serde_json::Value) -> Option<Self> {
        ArtifactFields::from_delivered_data(data).map(ArtifactFields::into_owned)
    }

    /// Admit the complete encoded artifact before copying any source fields.
    /// Returns `None` for invalid metadata or an over-budget artifact. This
    /// projection never changes the source's delivery state or owns its receipt.
    pub fn from_delivered_data_with_limit(data: &serde_json::Value, limit: usize) -> Option<Self> {
        let fields = ArtifactFields::from_delivered_data(data)?;
        crate::serialization::encoded_size(&fields, limit)?;
        Some(fields.into_owned())
    }
}

#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// A text chunk from the LLM response (may arrive many times).
    Chunk {
        delta: String,
    },
    /// A reasoning/thinking chunk from a thinking model (may arrive many times).
    Thinking {
        delta: String,
    },
    /// The agent is invoking a tool.
    ToolCall {
        /// Stable correlation ID shared with the matching [`TurnEvent::ToolResult`].
        id: String,
        name: String,
        args: serde_json::Value,
    },
    /// A tool has returned a result.
    ToolResult {
        /// Stable correlation ID shared with the originating [`TurnEvent::ToolCall`].
        id: String,
        name: String,
        output: String,
        /// Typed metadata for a file-producing tool (e.g. `deliver_file`), so
        /// channels attach the file structurally instead of parsing `output`.
        /// `None` for ordinary tools.
        artifact: Option<ToolArtifact>,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    ApprovalRequest {
        /// Correlation ID. The matching response frame must echo it.
        request_id: String,
        tool_name: String,
        /// Human-readable, secret-redacted summary of the tool arguments.
        /// Synthesised by `crate::approval::summarize_args`; never the raw
        /// `args` value.
        arguments_summary: String,
        /// How long the channel will wait before auto-denying.
        timeout_secs: u64,
    },
    /// Older whole turns were dropped to fit either the context token budget or
    /// the configured message limit. Surfaces a user-visible "context was cut
    /// here" marker so trimming is never silent. Emitted whenever a trim occurs.
    HistoryTrimmed {
        dropped_messages: usize,
        kept_turns: usize,
        reason: String,
    },
    /// Per-LLM-call token usage and cost; a turn may emit several, one per
    /// model call. `None` means "unavailable for this call", not zero.
    Usage {
        input_tokens: Option<u64>,
        /// Tokens served from the provider's prompt cache (e.g. Anthropic
        /// `cache_read_input_tokens`, OpenAI `cached_tokens`). These count
        /// toward the context window and must be added to `input_tokens` to
        /// get the true total context size.
        cached_input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cost_usd: Option<f64>,
    },
}

#[cfg(test)]
mod plan_event_tests {
    use super::*;
    use crate::plan::{PlanEntry, PlanPriority, PlanStatus};

    #[test]
    fn plan_turn_event_carries_entries() {
        let ev = TurnEvent::Plan {
            entries: vec![PlanEntry {
                content: "Step one".to_string(),
                status: PlanStatus::Pending,
                priority: PlanPriority::Medium,
                active_form: None,
            }],
        };
        match ev {
            TurnEvent::Plan { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].content, "Step one");
            }
            _ => panic!("expected TurnEvent::Plan"),
        }
    }
}

#[cfg(test)]
mod tool_artifact_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bounded_artifact_matches_owned_encoding_at_the_exact_limit() {
        for data in [
            json!({"delivered": true, "path": "/synthetic/minimal"}),
            json!({
                "delivered": true, "path": "/synthetic/😀\"\n",
                "uri": "attachment://fixture/\0", "filename": "fixture.txt",
                "title": "fixture \"title\"", "mimeType": "text/plain", "bytes": u64::MAX,
            }),
        ] {
            let artifact = ToolArtifact::from_delivered_data(&data).unwrap();
            let encoded = serde_json::to_vec(&artifact).unwrap();
            assert_eq!(
                ToolArtifact::from_delivered_data_with_limit(&data, encoded.len()),
                Some(artifact)
            );
            assert!(
                ToolArtifact::from_delivered_data_with_limit(&data, encoded.len() - 1).is_none()
            );
            assert!(ToolArtifact::from_delivered_data_with_limit(&data, 0).is_none());
            let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 6);
            assert!(value.get("mime").is_some());
            assert!(value.get("size").is_some());
        }
    }

    #[test]
    fn artifact_limit_counts_escaping_in_every_string_field() {
        for field in ["path", "uri", "filename", "title", "mimeType"] {
            let mut data = json!({"delivered": true, "path": "/synthetic/artifact"});
            data[field] = json!("\0".repeat(12_000));
            assert!(
                ToolArtifact::from_delivered_data_with_limit(&data, 64 * 1024).is_none(),
                "uncapped {field}"
            );
            assert_eq!(data[field].as_str().unwrap().len(), 12_000);
        }
    }

    #[test]
    fn artifact_limit_counts_all_fields_together() {
        let mut data = json!({"delivered": true, "bytes": u64::MAX});
        for field in ["path", "uri", "filename", "title", "mimeType"] {
            data[field] = json!("x".repeat(14_000));
        }
        assert!(ToolArtifact::from_delivered_data_with_limit(&data, 64 * 1024).is_none());
    }

    #[test]
    fn projects_delivered_data_into_typed_fields() {
        let data = json!({
            "delivered": true,
            "uri": "attachment://deliver/report.pdf",
            "path": "/ws/uploads/ab.pdf",
            "filename": "report.pdf",
            "title": "Quarterly report",
            "mimeType": "application/pdf",
            "bytes": 1234,
        });
        let a = ToolArtifact::from_delivered_data(&data).expect("delivered data yields artifact");
        assert_eq!(a.path, "/ws/uploads/ab.pdf");
        assert_eq!(a.uri, "attachment://deliver/report.pdf");
        assert_eq!(a.filename, "report.pdf");
        assert_eq!(a.title, "Quarterly report");
        assert_eq!(a.mime, "application/pdf");
        assert_eq!(a.size, 1234);
    }

    #[test]
    fn non_delivered_data_is_ignored() {
        // Ordinary structured tool output must not be mistaken for a file artifact.
        assert!(ToolArtifact::from_delivered_data(&json!({"result": 42})).is_none());
        assert!(
            ToolArtifact::from_delivered_data(&json!({"delivered": false, "path": "/x"})).is_none()
        );
    }

    #[test]
    fn delivered_without_path_is_ignored() {
        assert!(ToolArtifact::from_delivered_data(&json!({"delivered": true})).is_none());
        assert!(
            ToolArtifact::from_delivered_data(&json!({"delivered": true, "path": ""})).is_none()
        );
    }
}
