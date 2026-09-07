//! Shared TOML comment-writing helpers used by both the gateway HTTP CRUD
//! handlers and the CLI `zeroclaw config set --comment` / `zeroclaw config patch`
//! flow. Walks a `toml_edit::DocumentMut` to a leaf key by dotted path and
//! decorates its leading whitespace with `# {comment}\n`. Empty comment string

use std::path::Path;

pub async fn apply_comments(
    config_path: &Path,
    annotations: &[(String, String)],
) -> Result<(), std::io::Error> {
    if annotations.is_empty() {
        return Ok(());
    }
    let raw = tokio::fs::read_to_string(config_path).await?;
    // Parse errors may contain source excerpts (including credentials). Return
    // a content-free error and preserve the original file for repair.
    let mut doc: toml_edit::DocumentMut = raw.parse().map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Invalid config TOML for annotation",
        )
    })?;
    for (path, comment) in annotations {
        decorate_key(doc.as_table_mut(), path, comment);
    }
    crate::schema::write_config_atomically(config_path, &doc.to_string())
        .await
        .map_err(std::io::Error::other)
}

/// Walk to the leaf key for `dotted` and decorate it with `# {comment}\n`,
/// preserving any non-comment whitespace already in the prefix. Empty comment
/// strips comment lines from the existing prefix while leaving blank lines.
pub fn decorate_key(root: &mut toml_edit::Table, dotted: &str, comment: &str) {
    let segments: Vec<&str> = dotted.split('.').collect();
    let (last, rest) = match segments.split_last() {
        Some(s) => s,
        None => return,
    };
    fn walk<'a>(
        table: &'a mut toml_edit::Table,
        segs: &[&str],
    ) -> Option<&'a mut toml_edit::Table> {
        let mut cursor = table;
        for seg in segs {
            cursor = cursor.get_mut(seg)?.as_table_mut()?;
        }
        Some(cursor)
    }
    let table = match walk(root, rest) {
        Some(t) => t,
        None => return,
    };
    if let Some(mut key) = table.key_mut(last) {
        let decor = key.leaf_decor_mut();
        let new_prefix = build_comment_prefix(decor.prefix(), comment);
        decor.set_prefix(new_prefix);
    }
}

/// Build the new leading decor for a leaf, applying `# {comment}\n` while
/// preserving any non-comment whitespace already in the prefix. Empty `comment`
/// strips `#`-prefixed lines from the existing prefix.
pub fn build_comment_prefix(existing: Option<&toml_edit::RawString>, comment: &str) -> String {
    let prev = existing.and_then(|r| r.as_str()).unwrap_or("");
    let mut kept = String::new();
    for line in prev.split_inclusive('\n') {
        if !line.trim_start().starts_with('#') {
            kept.push_str(line);
        }
    }
    if !comment.is_empty() {
        for line in comment.lines() {
            kept.push_str("# ");
            kept.push_str(line);
            kept.push('\n');
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_config_is_an_explicit_private_error_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "private_fixture = \"UNFINISHED_PRIVATE_VALUE";
        tokio::fs::write(&path, original).await.unwrap();
        let error = apply_comments(&path, &[("private_fixture".into(), "note".into())])
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("UNFINISHED_PRIVATE_VALUE"));
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failed_annotation_backup_preserves_live_values_and_private_temp_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "schema_version = 3\n[private_fixture]\nvalue = \"unchanged\"\n";
        tokio::fs::write(&path, original).await.unwrap();
        // Force a pre-replacement storage failure through the real writer.
        tokio::fs::create_dir(dir.path().join("config.toml.bak"))
            .await
            .unwrap();
        assert!(
            apply_comments(&path, &[("private_fixture.value".into(), "note".into())])
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let temporary = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|entry| {
                    entry
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".config.toml.tmp-")
                })
                .expect("the failure occurred after the temporary write");
            assert_eq!(
                std::fs::metadata(temporary).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[tokio::test]
    async fn annotation_uses_canonical_post_replace_uncertainty_and_preserves_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "schema_version = 3\n[private_fixture]\nvalue = \"unchanged\"\n";
        tokio::fs::write(&path, original).await.unwrap();
        crate::schema::arm_post_replace_sync_failure_for_test(&path);
        apply_comments(
            &path,
            &[("private_fixture.value".into(), "new annotation".into())],
        )
        .await
        .unwrap();
        assert!(!crate::schema::post_replace_sync_failure_armed(&path));
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(after.contains("# new annotation"));
        assert_eq!(
            after.parse::<toml::Table>().unwrap(),
            original.parse::<toml::Table>().unwrap()
        );
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("config.toml.bak"))
                .await
                .unwrap(),
            original
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn annotation_replaces_inode_instead_of_truncating_open_readers() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "schema_version = 3\nvalue = 42\n";
        tokio::fs::write(&path, original).await.unwrap();
        let mut reader = std::fs::File::open(&path).unwrap();
        apply_comments(&path, &[("value".into(), "note".into())])
            .await
            .unwrap();
        let mut old_inode = String::new();
        reader.read_to_string(&mut old_inode).unwrap();
        assert_eq!(old_inode, original);
        assert!(
            tokio::fs::read_to_string(&path)
                .await
                .unwrap()
                .contains("# note")
        );
    }

    #[tokio::test]
    async fn invalid_replacement_never_touches_live_file_or_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup = dir.path().join("config.toml.bak");
        tokio::fs::write(&path, "value = 42\n").await.unwrap();
        tokio::fs::write(&backup, "value = 41\n").await.unwrap();
        let error = crate::schema::write_config_atomically(&path, "value = \"PRIVATE_INCOMPLETE")
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("PRIVATE_INCOMPLETE"));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "value = 42\n"
        );
        assert_eq!(
            tokio::fs::read_to_string(&backup).await.unwrap(),
            "value = 41\n"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn build_comment_prefix_appends_to_blank() {
        assert_eq!(build_comment_prefix(None, "why"), "# why\n");
    }

    #[test]
    fn build_comment_prefix_replaces_existing_comment() {
        let raw = toml_edit::RawString::from("\n# old\n");
        let out = build_comment_prefix(Some(&raw), "new");
        assert!(out.contains("# new\n"));
        assert!(!out.contains("old"));
        assert!(out.starts_with('\n')); // blank line preserved
    }

    #[test]
    fn build_comment_prefix_empty_strips() {
        let raw = toml_edit::RawString::from("\n# stale\n");
        let out = build_comment_prefix(Some(&raw), "");
        assert!(!out.contains('#'));
        assert_eq!(out, "\n");
    }

    #[test]
    fn build_comment_prefix_preserves_multi_blank_lines() {
        let raw = toml_edit::RawString::from("\n\n# inline\n");
        let out = build_comment_prefix(Some(&raw), "fresh");
        assert!(out.starts_with("\n\n"));
        assert!(out.contains("# fresh\n"));
        assert!(!out.contains("inline"));
    }

    #[test]
    fn build_comment_prefix_handles_multiline_comment() {
        let out = build_comment_prefix(None, "first\nsecond\nthird");
        assert_eq!(out, "# first\n# second\n# third\n");
    }
}
