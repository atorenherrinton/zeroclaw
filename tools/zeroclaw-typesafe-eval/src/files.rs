//! Owner-only artifacts; refuse symlinks, hard links, special files and overwrite.
use crate::{MAX_BYTES, MAX_LINE};
use anyhow::{Result, ensure};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

fn ancestors(path: &Path) -> Result<()> {
    for part in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        let meta = fs::symlink_metadata(part)?;
        ensure!(!meta.file_type().is_symlink(), "symlink_path");
    }
    Ok(())
}

pub fn private_dir(path: &Path) -> Result<()> {
    ensure!(cfg!(unix), "unix_required");
    ancestors(path)?;
    let meta = fs::metadata(path)?;
    ensure!(meta.is_dir(), "not_directory");
    #[cfg(unix)]
    ensure!(
        meta.mode() & 0o077 == 0 && meta.uid() == unsafe { libc::geteuid() },
        "directory_not_private"
    );
    Ok(())
}

pub fn open_private(path: &Path) -> Result<File> {
    ensure!(cfg!(unix), "unix_required");
    ancestors(path)?;
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let f = opts.open(path)?;
    let meta = f.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= MAX_BYTES,
        "invalid_input_file"
    );
    #[cfg(unix)]
    ensure!(
        meta.mode() & 0o077 == 0 && meta.nlink() == 1 && meta.uid() == unsafe { libc::geteuid() },
        "file_not_private"
    );
    Ok(f)
}

pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    open_private(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= MAX_BYTES, "file_limit");
    Ok(data)
}

pub fn write_new(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    private_dir(dir)?;
    ensure!(
        !name.contains('/') && bytes.len() as u64 <= MAX_BYTES,
        "invalid_output"
    );
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let mut f = opts.open(dir.join(name))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

pub fn init(path: &Path) -> Result<()> {
    ensure!(cfg!(unix), "unix_required");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    ancestors(parent)?;
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path)?;
    let mut key = [0_u8; 32];
    File::open("/dev/urandom")?.read_exact(&mut key)?;
    write_new(path, "salt", &key)
}

pub fn lines(file: File) -> impl Iterator<Item = Result<String>> {
    let mut reader = BufReader::new(file);
    std::iter::from_fn(move || {
        let mut buf = Vec::new();
        match reader
            .by_ref()
            .take((MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) => None,
            Ok(_) if buf.len() > MAX_LINE => Some(Err(anyhow::Error::msg("line_limit"))),
            Ok(_) => Some(String::from_utf8(buf).map_err(|_| anyhow::Error::msg("invalid_utf8"))),
            Err(e) => Some(Err(e.into())),
        }
    })
}
