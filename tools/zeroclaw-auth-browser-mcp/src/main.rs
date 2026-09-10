//! Credential-free MCP protocol adapter around the approved, pinned browser core.
use anyhow::{Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
    sync::Mutex,
};
const MAX_REQUEST: usize = 128 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
type Pending = Arc<Mutex<HashSet<String>>>;

fn installation_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::Error::msg("Home directory is unavailable"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        bail!("Home directory must be absolute");
    }
    Ok(home.join(".zeroclaw"))
}

fn verify_core(root: &Path) -> Result<()> {
    let directory = root.join("extensions/auth-browser");
    let pin = std::fs::read_to_string(directory.join("core.sha256"))
        .map_err(|_| anyhow::Error::msg("Approved browser-core fingerprint is unavailable"))?;
    let pin = pin.trim();
    if pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Approved browser-core fingerprint is invalid");
    }
    let binary = directory.join("zeroclaw-auth-browser");
    if std::fs::metadata(&binary)?.len() > 64 * 1024 * 1024 {
        bail!("Unexpected browser-core size");
    }
    let actual = format!("{:x}", Sha256::digest(std::fs::read(binary)?));
    if actual != pin {
        bail!(
            "The credential-owning browser core changed. Restore the approved signed core; do not reset Keychain permissions or reimport credentials automatically"
        );
    }
    Ok(())
}

async fn line<R: AsyncBufRead + Unpin>(reader: &mut R, max: usize) -> Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if data.is_empty() { None } else { Some(data) });
        }
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |p| p + 1);
        if data.len() + n > max {
            bail!("MCP line exceeds the configured limit");
        }
        data.extend_from_slice(&available[..n]);
        reader.consume(n);
        if data.last() == Some(&b'\n') {
            return Ok(Some(data));
        }
    }
}

fn normalized(mut response: Value, login_response: bool) -> Value {
    if !login_response
        || response.get("error").is_some()
        || response["result"]["content"].is_array()
    {
        return response;
    }
    let result = &response["result"];
    response["result"] = match (
        result["username_filled"].as_bool(),
        result["password_filled"].as_bool(),
    ) {
        (Some(username), Some(password)) => {
            let payload = json!({"username_filled":username,"password_filled":password});
            json!({"content":[{"type":"text","text":payload.to_string()}]})
        }
        _ => {
            json!({"isError":true,"content":[{"type":"text","text":"Invalid login completion response; inspect the page before retrying"}]})
        }
    };
    response
}

async fn requests<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    pending: Pending,
    input_closed: Arc<AtomicBool>,
) -> Result<()> {
    while let Some(mut data) = line(&mut reader, MAX_REQUEST).await? {
        if let Ok(value) = serde_json::from_slice::<Value>(&data)
            && value["method"] == "tools/call"
            && value["params"]["name"] == "login"
            && let Some(id) = value.get("id")
        {
            let mut ids = pending.lock().await;
            if ids.len() >= 128 {
                bail!("Too many pending login requests");
            }
            if !ids.insert(id.to_string()) {
                bail!("Duplicate pending login request ID");
            }
        }
        if data.last() != Some(&b'\n') {
            data.push(b'\n');
        }
        writer.write_all(&data).await?;
        writer.flush().await?;
    }
    input_closed.store(true, Ordering::SeqCst);
    writer.shutdown().await?;
    Ok(())
}

async fn responses<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    pending: Pending,
    input_closed: Arc<AtomicBool>,
) -> Result<()> {
    while let Some(data) = line(&mut reader, MAX_RESPONSE).await? {
        let value: Value = serde_json::from_slice(&data)
            .map_err(|_| anyhow::Error::msg("Browser core emitted invalid protocol data"))?;
        let is_login = if let Some(id) = value.get("id") {
            pending.lock().await.remove(&id.to_string())
        } else {
            false
        };
        let mut output = serde_json::to_vec(&normalized(value, is_login))?;
        output.push(b'\n');
        writer.write_all(&output).await?;
        writer.flush().await?;
    }
    if !input_closed.load(Ordering::SeqCst) {
        bail!("Browser core closed its output unexpectedly");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args_os().len() != 1 {
        bail!("This MCP adapter accepts no command arguments");
    }
    let root = installation_root()?;
    verify_core(&root)?;
    let mut child = Command::new(root.join("bin/zeroclaw-signed-launch"))
        .arg("auth-browser")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::Error::msg("Core input is unavailable"))?;
    let output = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::Error::msg("Core output is unavailable"))?;
    let pending = Arc::new(Mutex::new(HashSet::new()));
    let input_closed = Arc::new(AtomicBool::new(false));
    let relay = async {
        tokio::try_join!(
            requests(
                BufReader::new(tokio::io::stdin()),
                input,
                pending.clone(),
                input_closed.clone()
            ),
            responses(
                BufReader::new(output),
                tokio::io::stdout(),
                pending,
                input_closed
            )
        )?;
        Ok::<(), anyhow::Error>(())
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (result, interrupted) = tokio::select! {
        result = relay => (result,false),
        _ = term.recv() => (Ok(()),true),
        _ = tokio::signal::ctrl_c() => (Ok(()),true),
    };
    if interrupted || result.is_err() {
        let _ = child.start_kill();
    }
    let status = match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            child.kill().await?;
            child.wait().await?
        }
    };
    result?;
    if !interrupted && !status.success() {
        bail!("Browser core exited unexpectedly");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_login_booleans_enter_mcp_content() {
        let response = normalized(
            json!({"jsonrpc":"2.0","id":7,"result":{"username_filled":true,"password_filled":false,"secret":"dummy-not-for-output"}}),
            true,
        );
        assert_eq!(response["id"], 7);
        let payload: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(
            payload,
            json!({"username_filled":true,"password_filled":false})
        );
        assert!(!response.to_string().contains("dummy-not-for-output"));
    }
    #[test]
    fn existing_content_errors_and_other_responses_are_preserved() {
        for value in [
            json!({"id":1,"error":{"code":-1,"message":"error"}}),
            json!({"id":1,"result":{"content":[{"type":"text","text":"already valid"}],"isError":true}}),
        ] {
            assert_eq!(normalized(value.clone(), true), value);
        }
        let other = json!({"id":"init","result":{"serverInfo":{"name":"auth-browser"}}});
        assert_eq!(normalized(other.clone(), false), other);
    }
    #[test]
    fn malformed_login_result_fails_without_reflecting_fields() {
        let result = normalized(
            json!({"id":1,"result":{"username_filled":"dummy-secret","password_filled":true}}),
            true,
        );
        assert_eq!(result["result"]["isError"], true);
        assert!(!result.to_string().contains("dummy-secret"));
    }
    #[tokio::test]
    async fn bounds_are_enforced_before_forwarding() {
        assert!(line(&mut BufReader::new(&b"12345\n"[..]), 4).await.is_err());
        let mut reader = BufReader::new(&b"first\nsecond\n"[..]);
        assert_eq!(line(&mut reader, 20).await.unwrap().unwrap(), b"first\n");
        assert_eq!(line(&mut reader, 20).await.unwrap().unwrap(), b"second\n");
        assert!(line(&mut reader, 20).await.unwrap().is_none());
    }
}
