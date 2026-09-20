use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const MAX_TEXT: usize = 100_000;

pub fn id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid exact ID"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Read {
    DriveList {
        name_contains: Option<String>,
        page_token: Option<String>,
    },
    DriveMetadata {
        file_id: String,
    },
    DocsRead {
        document_id: String,
    },
    /// Compare exact UTF-8 body text, including Google's final newline, in one tab.
    DocsVerify {
        document_id: String,
        tab_id: String,
        expected_text_sha256: String,
        expected_revision_id: Option<String>,
    },
}
impl Read {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::DriveList {
                name_contains,
                page_token,
            } => {
                if let Some(name) = name_contains {
                    ensure!(
                        !name.is_empty()
                            && name.len() <= 200
                            && !name.chars().any(char::is_control),
                        "invalid search text"
                    );
                }
                if let Some(token) = page_token {
                    ensure!(
                        !token.is_empty()
                            && token.len() <= 2048
                            && !token.chars().any(char::is_control),
                        "invalid page token"
                    );
                }
            }
            Self::DriveMetadata { file_id } => id(file_id)?,
            Self::DocsRead { document_id } => id(document_id)?,
            Self::DocsVerify {
                document_id,
                tab_id,
                expected_text_sha256,
                expected_revision_id,
            } => {
                id(document_id)?;
                // Docs tab IDs use periods (for example t.0); they never enter URLs.
                ensure!(
                    !tab_id.is_empty()
                        && tab_id.len() <= 200
                        && tab_id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                    "invalid exact tab ID"
                );
                ensure!(
                    expected_text_sha256.len() == 64
                        && expected_text_sha256
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "expected lowercase SHA256 required"
                );
                if let Some(revision) = expected_revision_id {
                    ensure!(
                        !revision.is_empty()
                            && revision.len() <= 2000
                            && !revision.chars().any(char::is_control),
                        "invalid revision ID"
                    );
                }
            }
        }
        Ok(())
    }
}
