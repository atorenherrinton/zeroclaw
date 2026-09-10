//! Local Apple Passwords import and exact-origin credential lookup.
//! Passwords live only in the macOS Keychain; the index contains account metadata.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const SERVICE: &str = "com.zeroclaw.local.auth-browser";
const MAX_CSV_BYTES: u64 = 32 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ACCOUNTS: usize = 20_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub id: String,
    pub origin: String,
    pub username: String,
}

#[derive(Debug, Default, Serialize)]
pub struct ImportSummary {
    pub imported: usize,
    pub skipped: usize,
}

#[derive(Debug, Serialize)]
pub struct StatusSummary {
    pub accounts: usize,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    accounts: Vec<Account>,
}

// Deliberately no Debug or Serialize implementation for a credential.
struct ImportedCredential {
    account: Account,
    password: Zeroizing<String>,
}

fn safe_error(message: &'static str) -> anyhow::Error {
    anyhow::Error::msg(message)
}

fn origin_for_url(raw: &str, allow_http_upgrade: bool) -> Result<String> {
    let mut url = url::Url::parse(raw.trim())
        .map_err(|_| safe_error("Invalid website URL; credential was not used"))?;
    if allow_http_upgrade && url.scheme() == "http" {
        if url.port().is_some_and(|port| port != 80) {
            bail!("Unsupported website port; credential was not used");
        }
        url.set_port(None)
            .map_err(|_| safe_error("Invalid website URL; credential was not used"))?;
        url.set_scheme("https")
            .map_err(|_| safe_error("Invalid website URL; credential was not used"))?;
    }
    crate::policy::validate_url(url.as_str())
        .map_err(|_| safe_error("Unsupported website origin; credential was not used"))?;
    // A terminal DNS dot is equivalent for lookup but must not create a second
    // credential origin. URL parsing already canonicalizes case and IDNs.
    let host = url
        .host_str()
        .ok_or_else(|| safe_error("Website hostname required"))?
        .trim_end_matches('.');
    Ok(format!("https://{host}"))
}

fn account_id(origin: &str, username: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(origin.as_bytes());
    hash.update([0]);
    hash.update(username.as_bytes());
    format!("{:x}", hash.finalize())
}

fn validate_account(account: &Account) -> Result<()> {
    let origin = origin_for_url(&account.origin, false)?;
    if origin != account.origin
        || account.id != account_id(&origin, &account.username)
        || account.username.len() > 2048
        || account.username.chars().any(char::is_control)
    {
        bail!("Invalid credential account metadata");
    }
    Ok(())
}

fn parse_csv(data: &[u8]) -> Result<(Vec<ImportedCredential>, usize)> {
    if data.len() as u64 > MAX_CSV_BYTES {
        bail!("Password export exceeds the 32 MiB import limit");
    }
    let mut reader = csv::ReaderBuilder::new().from_reader(data);
    let headers = reader
        .headers()
        .map_err(|_| safe_error("Cannot read Apple Passwords CSV headers"))?;
    let position = |name: &str| -> Result<usize> {
        let positions: Vec<_> = headers
            .iter()
            .enumerate()
            .filter(|(_, h)| {
                h.trim_start_matches('\u{feff}')
                    .trim()
                    .eq_ignore_ascii_case(name)
            })
            .map(|(i, _)| i)
            .collect();
        if positions.len() != 1 {
            bail!("Expected Apple Passwords CSV with unique URL, Username, and Password columns");
        }
        Ok(positions[0])
    };
    let url_column = position("URL")?;
    let username_column = position("Username")?;
    let password_column = position("Password")?;
    let mut rows = BTreeMap::new();
    let mut skipped = 0;
    for (row_number, record) in reader.records().enumerate() {
        if row_number >= 100_000 {
            bail!("Password export exceeds the row import limit");
        }
        let record = record.map_err(|_| {
            safe_error("Malformed Apple Passwords CSV; no credentials were imported")
        })?;
        let username = record.get(username_column).unwrap_or_default();
        let password = record.get(password_column).unwrap_or_default();
        let origin = origin_for_url(record.get(url_column).unwrap_or_default(), true);
        if origin.is_err()
            || username.len() > 2048
            || username.chars().any(char::is_control)
            || password.is_empty()
            || password.len() > 16_384
            || password.contains('\0')
        {
            skipped += 1;
            continue;
        }
        let origin = origin?;
        let id = account_id(&origin, username);
        let entry = ImportedCredential {
            account: Account {
                id: id.clone(),
                origin,
                username: username.into(),
            },
            password: Zeroizing::new(password.into()),
        };
        if rows.insert(id, entry).is_some() {
            skipped += 1;
        }
        if rows.len() > MAX_ACCOUNTS {
            bail!("Password export exceeds the account import limit");
        }
    }
    Ok((rows.into_values().collect(), skipped))
}

fn vault_dir() -> Result<PathBuf> {
    let home =
        std::env::var_os("HOME").ok_or_else(|| safe_error("Home directory is unavailable"))?;
    let path = PathBuf::from(home).join(".zeroclaw/auth-browser");
    fs::create_dir_all(&path)
        .map_err(|_| safe_error("Cannot create private credential index directory"))?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| safe_error("Cannot inspect credential index directory"))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("Credential index directory must be an owned directory, not a symlink");
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
        .map_err(|_| safe_error("Cannot secure credential index directory"))?;
    Ok(path)
}

struct IndexLock(File);

impl IndexLock {
    fn acquire(dir: &Path, exclusive: bool) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("index.lock"))
            .map_err(|_| safe_error("Cannot lock credential index"))?;
        let metadata = file
            .metadata()
            .map_err(|_| safe_error("Cannot inspect credential index lock"))?;
        if !metadata.is_file()
            || metadata.mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            bail!("Credential index lock has unsafe ownership or permissions");
        }
        let operation = if exclusive {
            libc::LOCK_EX
        } else {
            libc::LOCK_SH
        };
        if unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) } != 0 {
            bail!("Credential index is busy; try again when the other operation finishes");
        }
        Ok(Self(file))
    }
}

impl Drop for IndexLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn read_index(dir: &Path) -> Result<Index> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("accounts.json"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
        Err(_) => bail!("Cannot read credential account index"),
    };
    let metadata = file
        .metadata()
        .map_err(|_| safe_error("Cannot inspect credential account index"))?;
    if !metadata.is_file()
        || metadata.len() > MAX_INDEX_BYTES
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("Credential account index has unsafe permissions or size");
    }
    let index: Index = serde_json::from_reader(file.take(MAX_INDEX_BYTES + 1))
        .map_err(|_| safe_error("Invalid credential account index"))?;
    if index.accounts.len() > MAX_ACCOUNTS {
        bail!("Credential account index exceeds account limit");
    }
    let mut ids = std::collections::BTreeSet::new();
    for account in &index.accounts {
        validate_account(account)?;
        if !ids.insert(&account.id) {
            bail!("Credential account index contains duplicate accounts");
        }
    }
    Ok(index)
}

fn write_index(dir: &Path, index: &Index) -> Result<()> {
    let data = serde_json::to_vec(index)
        .map_err(|_| safe_error("Cannot encode credential account index"))?;
    if data.len() as u64 > MAX_INDEX_BYTES {
        bail!("Credential account index exceeds size limit");
    }
    let temporary = dir.join(format!("accounts.json.{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .map_err(|_| safe_error("Cannot stage private credential account index"))?;
    let result = (|| {
        file.write_all(&data)
            .map_err(|_| safe_error("Cannot write credential account index"))?;
        file.sync_all()
            .map_err(|_| safe_error("Cannot save credential account index"))?;
        fs::rename(&temporary, dir.join("accounts.json"))
            .map_err(|_| safe_error("Cannot install credential account index"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Local setup only. Never expose this importer as an MCP tool.
pub fn import_csv(path: &Path) -> Result<ImportSummary> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| safe_error("Cannot open local Apple Passwords export"))?;
    let metadata = file
        .metadata()
        .map_err(|_| safe_error("Cannot inspect local password export"))?;
    if !metadata.is_file() || metadata.len() > MAX_CSV_BYTES {
        bail!("Password export must be a regular CSV file no larger than 32 MiB");
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| safe_error("Cannot inspect password export directory"))?;
    if metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
        || !parent_metadata.is_dir()
        || parent_metadata.file_type().is_symlink()
        || parent_metadata.mode() & 0o077 != 0
        || parent_metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("Password export must be owner-only (0600) inside an owner-only directory (0700)");
    }
    let mut data = Zeroizing::new(Vec::new());
    file.take(MAX_CSV_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|_| safe_error("Cannot read local Apple Passwords export"))?;
    let (credentials, skipped) = parse_csv(&data)?;
    let dir = vault_dir()?;
    let _lock = IndexLock::acquire(&dir, true)?;
    let mut index = read_index(&dir)?;
    let new_count = credentials
        .iter()
        .filter(|credential| !index.accounts.iter().any(|a| a.id == credential.account.id))
        .count();
    if index.accounts.len() + new_count > MAX_ACCOUNTS {
        bail!("Credential account index exceeds account limit");
    }
    let mut summary = ImportSummary {
        imported: 0,
        skipped,
    };
    for credential in credentials {
        let position = index
            .accounts
            .iter()
            .position(|a| a.id == credential.account.id);
        if let Err(error) = store(&credential.account, &credential.password) {
            // Keep completed imports discoverable after a later Keychain error.
            // The export remains available for a safe, idempotent retry.
            write_index(&dir, &index)?;
            return Err(error);
        }
        let verification = load(&credential.account).and_then(|actual| {
            if actual.as_str() == credential.password.as_str() {
                Ok(())
            } else {
                bail!(
                    "Stored credential verification failed; keep the local export and retry setup"
                )
            }
        });
        if let Some(position) = position {
            index.accounts[position] = credential.account;
        } else {
            index.accounts.push(credential.account);
        }
        if let Err(error) = verification {
            write_index(&dir, &index)?;
            return Err(error);
        }
        summary.imported += 1;
    }
    write_index(&dir, &index)?;
    Ok(summary)
}

pub fn status() -> Result<StatusSummary> {
    let dir = vault_dir()?;
    let _lock = IndexLock::acquire(&dir, false)?;
    Ok(StatusSummary {
        accounts: read_index(&dir)?.accounts.len(),
    })
}

pub fn accounts_for_origin(raw: &str) -> Result<Vec<Account>> {
    let origin = origin_for_url(raw, false)?;
    let dir = vault_dir()?;
    let _lock = IndexLock::acquire(&dir, false)?;
    Ok(read_index(&dir)?
        .accounts
        .into_iter()
        .filter(|account| account.origin == origin)
        .collect())
}

#[cfg(target_os = "macos")]
fn keychain_error(code: i32) -> anyhow::Error {
    safe_error(match code {
        -25300 => "Credential is not available in Keychain; import it locally",
        -25308 | -25293 => {
            "Credential Keychain is locked or access was denied; complete local access setup"
        }
        _ => "Credential Keychain operation failed; no secret was returned",
    })
}

#[cfg(target_os = "macos")]
fn store(account: &Account, password: &str) -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_account(account)?;
    let keychain = SecKeychain::default().map_err(|e| keychain_error(e.code()))?;
    // Creator-app ACL only: import through the installed, certificate-signed
    // helper so its stable identity can later retrieve this item unattended.
    keychain
        .set_generic_password(SERVICE, &account.id, password.as_bytes())
        .map_err(|e| keychain_error(e.code()))
}

#[cfg(target_os = "macos")]
pub fn load(account: &Account) -> Result<Zeroizing<String>> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_account(account)?;
    let _guard = SecKeychain::disable_user_interaction().map_err(|e| keychain_error(e.code()))?;
    let keychain = SecKeychain::default().map_err(|e| keychain_error(e.code()))?;
    let (password, _) = keychain
        .find_generic_password(SERVICE, &account.id)
        .map_err(|e| keychain_error(e.code()))?;
    let bytes = Zeroizing::new(password.to_vec());
    let secret = std::str::from_utf8(&bytes)
        .map_err(|_| safe_error("Credential has an unsupported format"))?;
    if secret.is_empty() || secret.len() > 16_384 || secret.contains('\0') {
        bail!("Credential has an unsupported format");
    }
    Ok(Zeroizing::new(secret.to_owned()))
}

#[cfg(not(target_os = "macos"))]
fn store(_: &Account, _: &str) -> Result<()> {
    bail!("Credential storage requires macOS Keychain")
}

#[cfg(not(target_os = "macos"))]
pub fn load(_: &Account) -> Result<Zeroizing<String>> {
    bail!("Credential storage requires macOS Keychain")
}

/// Local CLI verification only: creates and removes one synthetic credential.
#[cfg(target_os = "macos")]
pub fn self_test_keychain() -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    const TEST_SERVICE: &str = "com.zeroclaw.local.auth-browser.self-test";
    let account = format!(
        "test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| safe_error("Synthetic Keychain test clock unavailable"))?
            .as_nanos()
    );
    let keychain =
        SecKeychain::default().map_err(|_| safe_error("Synthetic test Keychain unavailable"))?;
    let _guard = SecKeychain::disable_user_interaction()
        .map_err(|_| safe_error("Synthetic Keychain test could not disable interaction"))?;
    let fixture = b"synthetic-auth-browser-credential-not-a-real-password";
    keychain
        .add_generic_password(TEST_SERVICE, &account, fixture)
        .map_err(|_| safe_error("Synthetic Keychain test creation failed"))?;
    let result = match keychain.find_generic_password(TEST_SERVICE, &account) {
        Ok((actual, item)) => {
            let matches = actual.as_ref() == fixture;
            item.delete();
            if matches {
                Ok(())
            } else {
                Err(safe_error("Synthetic Keychain test value mismatch"))
            }
        }
        Err(_) => {
            // Remove only this uniquely named synthetic test item even if its
            // initial noninteractive password read was denied.
            let _ = security_framework::passwords::delete_generic_password(TEST_SERVICE, &account);
            Err(safe_error(
                "Synthetic Keychain test could not read without interaction",
            ))
        }
    };
    match keychain.find_generic_password(TEST_SERVICE, &account) {
        Err(error) if error.code() == -25300 => (),
        _ => bail!("Synthetic Keychain test cleanup could not be verified"),
    }
    result
}

#[cfg(not(target_os = "macos"))]
pub fn self_test_keychain() -> Result<()> {
    bail!("Credential storage requires macOS Keychain")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_csv_handles_quotes_newlines_and_http_upgrade_without_secret_metadata() {
        let data = b"Title,URL,Username,Password,Notes,OTPAuth\nTest,http://EXAMPLE.com/login,user@example.com,\"synthetic,secret\nline\",note,\n";
        let (rows, skipped) = parse_csv(data).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].account.origin, "https://example.com");
        assert_eq!(&*rows[0].password, "synthetic,secret\nline");
        assert!(!format!("{:?}", rows[0].account).contains("synthetic"));
        assert!(
            !serde_json::to_string(&rows[0].account)
                .unwrap()
                .contains("synthetic")
        );
    }

    #[test]
    fn exact_origins_and_account_ids_do_not_cross_domains_or_users() {
        assert_eq!(
            origin_for_url("https://EXAMPLE.com.:443/login", false).unwrap(),
            "https://example.com"
        );
        assert_ne!(
            origin_for_url("https://accounts.example.com/", false).unwrap(),
            "https://example.com"
        );
        assert_ne!(
            account_id("https://example.com", "alice"),
            account_id("https://example.com", "bob")
        );
        assert_ne!(
            account_id("https://example.com", "alice"),
            account_id("https://evil.example.com", "alice")
        );
        assert!(origin_for_url("http://example.com", false).is_err());
    }

    #[test]
    fn malicious_and_unsupported_urls_are_skipped_and_never_reflected() {
        for url in [
            "https://example.com@evil.example.com",
            "https://localhost",
            "https://10.0.0.1",
            "https://example.com:8443",
            "https://linkedin.com",
            "javascript:secret-value",
            "file:///secret-value",
            "http://example.com:8080",
        ] {
            let csv = format!("URL,Username,Password\n{url},test,synthetic-secret\n");
            let (rows, skipped) = parse_csv(csv.as_bytes()).unwrap();
            assert_eq!(skipped, 1);
            assert!(rows.is_empty());
        }
        let error = origin_for_url("javascript:synthetic-secret", false)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("synthetic-secret"));
    }

    #[test]
    fn duplicate_accounts_last_password_wins_but_other_accounts_survive() {
        let data = b"URL,Username,Password\nhttps://example.com/a,alice,one\nhttps://example.com/b,bob,two\nhttps://example.com/c,alice,three\n";
        let (rows, skipped) = parse_csv(data).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(skipped, 1);
        assert_eq!(
            &*rows
                .iter()
                .find(|r| r.account.username == "alice")
                .unwrap()
                .password,
            "three"
        );
    }

    #[test]
    fn malformed_headers_and_rows_have_sanitized_errors() {
        for data in [
            "URL,Username,Password,Password\n",
            "URL,Username\n",
            "URL,Username,Password\nhttps://example.com,user,password,synthetic-secret\n",
        ] {
            let result = parse_csv(data.as_bytes());
            assert!(result.is_err());
            assert!(
                !result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("synthetic-secret")
            );
        }
    }

    #[test]
    fn metadata_tampering_cannot_select_arbitrary_keychain_accounts() {
        let mut account = Account {
            id: account_id("https://example.com", "alice"),
            origin: "https://example.com".into(),
            username: "alice".into(),
        };
        assert!(validate_account(&account).is_ok());
        account.origin = "https://evil.example.com".into();
        assert!(validate_account(&account).is_err());
        account.origin = "https://example.com".into();
        account.id = "unrelated-keychain-item".into();
        assert!(validate_account(&account).is_err());
    }
}
