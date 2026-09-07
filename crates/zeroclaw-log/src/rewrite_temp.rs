//! Exclusive ownership of a private log rewrite file until replacement.
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};

pub(crate) struct RewriteTemp {
    path: PathBuf,
    armed: bool,
}

pub(crate) fn create(path: &Path) -> Result<(File, RewriteTemp)> {
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(path)
        .with_context(|| format!("creating log rewrite temp {}", path.display()))?;
    Ok((file, RewriteTemp::new(path.to_path_buf())))
}

impl RewriteTemp {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    pub(crate) fn commit(self, destination: &Path) -> Result<()> {
        self.commit_with_sync(destination, sync_parent_directory)
    }

    fn commit_with_sync(
        mut self,
        destination: &Path,
        sync: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        fs::rename(&self.path, destination).context("replacing log with synced rewrite")?;
        self.disarm();
        if let Err(error) = sync(destination) {
            // Replacement already happened. Never roll back or repeat writes.
            tracing::warn!(target: "zeroclaw_log", error = ?error,
                "log: rewrite committed but parent directory sync failed");
        }
        Ok(())
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RewriteTemp {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(target: "zeroclaw_log", error = ?error,
                "log: failed to remove uncommitted rewrite temp");
        }
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .with_context(|| format!("opening log directory for fsync: {}", parent.display()))?
            .sync_all()
            .with_context(|| format!("fsync log directory after rewrite: {}", parent.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_replace_sync_failure_keeps_committed_log_without_replay() {
        // The injected sync warning reaches the global capture subscriber.
        let _writer_guard = crate::writer::WRITER_TEST_LOCK.lock();
        let _hook_guard = crate::broadcast::HOOK_TEST_LOCK.lock();
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("trace.jsonl");
        let path = directory.path().join("trace.tmp.synthetic");
        fs::write(&destination, "old").unwrap();
        let (mut file, cleanup) = create(&path).unwrap();
        std::io::Write::write_all(&mut file, b"new").unwrap();
        file.sync_all().unwrap();
        drop(file);
        cleanup
            .commit_with_sync(&destination, |_| anyhow::bail!("synthetic sync refusal"))
            .unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "new");
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_temp_is_private_from_creation_and_symlinks_are_refused() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("trace.tmp.synthetic");
        let (file, cleanup) = create(&path).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(file);
        drop(cleanup);
        let unrelated = directory.path().join("unrelated");
        fs::write(&unrelated, "preserve").unwrap();
        symlink(&unrelated, &path).unwrap();
        assert!(create(&path).is_err());
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&unrelated).unwrap(), "preserve");
    }

    #[test]
    fn failed_replace_removes_temp_without_touching_destination() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("occupied");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker"), "preserved").unwrap();
        let path = directory.path().join("trace.tmp.synthetic");
        let (file, cleanup) = create(&path).unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert!(cleanup.commit(&destination).is_err());
        assert!(!path.exists());
        assert_eq!(
            fs::read_to_string(destination.join("marker")).unwrap(),
            "preserved"
        );
    }

    #[test]
    fn temp_cleanup_removes_only_uncommitted_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("migration.tmp");
        fs::write(&path, "pending").unwrap();
        {
            let _cleanup = RewriteTemp::new(path.clone());
        }
        assert!(!path.exists(), "uncommitted temp file must be removed");

        fs::write(&path, "committed").unwrap();
        {
            let mut cleanup = RewriteTemp::new(path.clone());
            cleanup.disarm();
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), "committed");
    }

    #[test]
    fn temp_creation_collision_preserves_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("migration.tmp");
        fs::write(&path, "owned elsewhere").unwrap();

        assert!(create(&path).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "owned elsewhere");
    }
}
