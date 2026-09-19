use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub const MAX_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FILES: usize = 10;
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn id(s: &str) -> Result<&str> {
    ensure!(
        !s.is_empty()
            && s.len() <= 160
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "exact provider ID required"
    );
    Ok(s)
}
pub fn key(s: &str) -> Result<&str> {
    id(s)
}
pub fn text<'a>(v: &'a Value, name: &str, max: usize) -> Result<&'a str> {
    let s = v[name]
        .as_str()
        .with_context(|| format!("{name} required"))?;
    ensure!(
        !s.is_empty() && s.len() <= max && !s.contains('\0'),
        "invalid {name}"
    );
    Ok(s)
}
pub fn fields(v: &Value, allowed: &[&str]) -> Result<()> {
    ensure!(
        v.as_object()
            .context("object required")?
            .keys()
            .all(|k| allowed.contains(&k.as_str())),
        "unexpected argument"
    );
    Ok(())
}
pub fn header(s: &str, max: usize) -> Result<&str> {
    ensure!(
        s.len() <= max && !s.chars().any(char::is_control),
        "invalid header value"
    );
    Ok(s)
}
pub fn mailbox(s: &str) -> Result<&str> {
    ensure!(
        s.len() <= 254 && s.is_ascii(),
        "bare ASCII mailbox required"
    );
    let (local, domain) = s.split_once('@').context("mailbox requires @")?;
    ensure!(
        !local.is_empty()
            && local.len() <= 64
            && !local.starts_with('.')
            && !local.ends_with('.')
            && !local.contains("..")
            && local
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&b)),
        "invalid mailbox local part"
    );
    ensure!(
        domain.contains('.')
            && domain.split('.').all(|p| !p.is_empty()
                && p.len() <= 63
                && !p.starts_with('-')
                && !p.ends_with('-')
                && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')),
        "invalid mailbox domain"
    );
    Ok(s)
}
pub fn addresses(v: Option<&Value>) -> Result<Vec<String>> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let list: Vec<String> = match v {
        Value::String(s) if s.is_empty() => Vec::new(),
        Value::String(s) => s.split(',').map(|s| s.trim().to_owned()).collect(),
        Value::Array(a) => a
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("mailbox string required")
            })
            .collect::<Result<_>>()?,
        _ => bail!("recipient array or legacy comma-separated string required"),
    };
    ensure!(list.len() <= 50, "at most 50 recipients");
    for s in &list {
        mailbox(s)?;
    }
    Ok(list)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub sha256: String,
    pub size: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Content {
    pub from: String,
    pub reply_to: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub html_body: Option<String>,
    pub attachments: Vec<Attachment>,
    pub thread_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub source_message_id: Option<String>,
    pub mode: String,
    pub message_id: String,
}
impl Content {
    pub fn validate(&self) -> Result<()> {
        mailbox(&self.from)?;
        if let Some(s) = &self.reply_to {
            mailbox(s)?;
        }
        let mut seen = HashSet::new();
        let all = self
            .to
            .iter()
            .chain(&self.cc)
            .chain(&self.bcc)
            .collect::<Vec<_>>();
        ensure!(
            (1..=50).contains(&all.len()),
            "1 to 50 exact recipients required"
        );
        for s in all {
            mailbox(s)?;
            ensure!(
                seen.insert(s.to_ascii_lowercase()),
                "duplicate recipient across To/CC/BCC"
            );
        }
        header(&self.subject, 998)?;
        ensure!(
            self.body.len() <= 100_000 && !self.body.contains('\0'),
            "body exceeds 100000 UTF-8 bytes or contains NUL"
        );
        ensure!(
            self.html_body
                .as_ref()
                .is_none_or(|s| s.len() <= 100_000 && !s.contains('\0')),
            "HTML body exceeds limit"
        );
        if let Some(s) = &self.thread_id {
            id(s)?;
        }
        if let Some(s) = &self.source_message_id {
            id(s)?;
        }
        if let Some(s) = &self.in_reply_to {
            message_id(s)?;
        }
        ensure!(self.references.len() <= 50, "reply references exceed limit");
        for s in &self.references {
            message_id(s)?;
        }
        message_id(&self.message_id)?;
        ensure!(
            self.attachments.len() <= MAX_FILES
                && self
                    .attachments
                    .iter()
                    .try_fold(0usize, |sum, a| sum.checked_add(a.size))
                    .is_some_and(|size| size <= MAX_BYTES),
            "attachment count/aggregate size bound exceeded"
        );
        for a in &self.attachments {
            filename(&a.filename)?;
            ensure!(
                a.size <= MAX_FILE_BYTES,
                "attachment per-file limit exceeded"
            );
            mime(&a.mime_type)?;
            ensure!(
                a.sha256.len() == 64 && a.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid content hash"
            );
        }
        ensure!(
            matches!(
                self.mode.as_str(),
                "new" | "reply" | "reply_all" | "forward" | "existing"
            ),
            "unsupported composition mode"
        );
        Ok(())
    }
    pub fn assemble(&self, blobs: &impl Fn(&str) -> Result<Vec<u8>>, date: i64) -> Result<Vec<u8>> {
        self.validate()?;
        let mut message = mail_builder::MessageBuilder::new()
            .from(self.from.as_str())
            .to(self.to.iter().map(String::as_str).collect::<Vec<_>>())
            .subject(self.subject.as_str())
            .text_body(self.body.as_str())
            .message_id(self.message_id.as_str())
            .date(date);
        if let Some(html) = &self.html_body {
            message = message.html_body(html.as_str());
        }
        if !self.cc.is_empty() {
            message = message.cc(self.cc.iter().map(String::as_str).collect::<Vec<_>>());
        }
        if !self.bcc.is_empty() {
            message = message.bcc(self.bcc.iter().map(String::as_str).collect::<Vec<_>>());
        }
        if let Some(v) = &self.reply_to {
            message = message.reply_to(v.as_str());
        }
        if let Some(v) = &self.in_reply_to {
            message = message.in_reply_to(v.as_str());
        }
        if !self.references.is_empty() {
            message = message.references(
                self.references
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            );
        }
        for a in &self.attachments {
            let bytes = blobs(&a.sha256)?;
            ensure!(
                hash(&bytes) == a.sha256 && bytes.len() == a.size,
                "immutable attachment corrupted"
            );
            message = message.attachment(a.mime_type.clone(), a.filename.clone(), bytes);
        }
        message.write_to_vec().context("MIME assembly failed")
    }
}
pub fn message_id(s: &str) -> Result<&str> {
    ensure!(
        s.len() <= 254
            && s.is_ascii()
            && s.contains('@')
            && s.bytes()
                .all(|b| b.is_ascii_graphic() && !b"<>(),;:\\\"[]".contains(&b)),
        "invalid RFC Message-ID"
    );
    Ok(s)
}
pub fn filename(s: &str) -> Result<&str> {
    header(s, 240)?;
    ensure!(
        !s.is_empty() && !s.contains(['/', '\\']) && !matches!(s, "." | ".."),
        "filename must be a safe basename"
    );
    Ok(s)
}
pub fn mime(s: &str) -> Result<&str> {
    let parts: Vec<_> = s.split('/').collect();
    ensure!(
        parts.len() == 2
            && parts.iter().all(|s| !s.is_empty()
                && s.len() <= 80
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))),
        "invalid MIME type"
    );
    Ok(s)
}
/// Open every component relative to an already-open directory, never following
/// symlinks (including internal links). NONBLOCK prevents FIFO open hangs.
pub fn read_file(path: &str, roots: &[PathBuf]) -> Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let path = Path::new(path);
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
        "exact approved-root file required"
    );
    ensure!(
        roots
            .iter()
            .any(|r| r.is_absolute() && path.starts_with(r) && path != r),
        "attachment outside approved roots; do not copy to evade policy"
    );
    let mut dir = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut components = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .peekable();
    while let Some(component) = components.next() {
        let last = components.peek().is_none();
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if !last {
            flags |= OFlags::DIRECTORY;
        }
        let fd = openat(&dir, component, flags, Mode::empty())
            .map_err(|_| anyhow::Error::msg("attachment unavailable or symlink denied"))?;
        if last {
            let file = std::fs::File::from(fd);
            let m = file.metadata().context("attachment metadata unavailable")?;
            ensure!(
                m.is_file() && m.len() <= MAX_FILE_BYTES as u64,
                "attachment must be a bounded regular file"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                ensure!(m.nlink() == 1, "hard-linked attachment denied");
            }
            let mut bytes = Vec::new();
            file.take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .context("attachment read failed")?;
            ensure!(
                bytes.len() <= MAX_FILE_BYTES,
                "attachment per-file limit exceeded"
            );
            return Ok(bytes);
        }
        dir = fd;
    }
    bail!("regular attachment file required")
}
pub fn decode_raw(v: &Value) -> Result<Vec<u8>> {
    let s = text(v, "raw", 24 * 1024 * 1024)?;
    URL_SAFE_NO_PAD
        .decode(s.trim_end_matches('='))
        .context("invalid provider MIME encoding")
}
