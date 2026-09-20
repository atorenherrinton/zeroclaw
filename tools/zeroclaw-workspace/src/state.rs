//! Private canonical credentials, immutable grants and dispatch receipts.
use anyhow::{Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub struct State {
    dir: PathBuf,
    _lock: File,
}
impl State {
    pub fn open(root: &Path) -> Result<Self> {
        ensure!(root.is_absolute(), "absolute state root required");
        for parent in root.ancestors() {
            ensure!(!parent.is_symlink(), "state symlink denied");
        }
        let dir = root.join("workspace-native-v1");
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                File::open(root)?.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        check(&dir, true)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("lock"))?;
        check(&dir.join("lock"), false)?;
        lock.try_lock()
            .map_err(|_| anyhow::Error::msg("Workspace operation already in progress"))?;
        Ok(Self { dir, _lock: lock })
    }
    pub fn read<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        crate::model::id(key)?;
        let path = self.dir.join(key);
        if !path.try_exists()? {
            ensure!(!path.is_symlink(), "state symlink denied");
            return Ok(None);
        }
        check(&path, false)?;
        let mut bytes = zeroize::Zeroizing::new(Vec::new());
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?
            .take(512 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512 * 1024, "state exceeds bound");
        Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
            anyhow::Error::msg("invalid private Workspace state")
        })?))
    }
    pub fn save<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        crate::model::id(key)?;
        let path = self.dir.join(key);
        ensure!(!path.is_symlink(), "state symlink denied");
        let tmp = self
            .dir
            .join(format!("temporary-{}", crate::consent::random()?));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        let bytes = zeroize::Zeroizing::new(serde_json::to_vec(value)?);
        ensure!(bytes.len() <= 512 * 1024, "state exceeds bound");
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(tmp, path)?;
        File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}
fn check(path: &Path, directory: bool) -> Result<()> {
    let m = std::fs::symlink_metadata(path)?;
    ensure!(
        !m.file_type().is_symlink()
            && m.is_dir() == directory
            && (directory || m.is_file())
            && m.uid() == unsafe { libc::geteuid() }
            && m.permissions().mode() & 0o077 == 0
            && (directory || m.nlink() == 1),
        "private owned Workspace state required"
    );
    Ok(())
}
