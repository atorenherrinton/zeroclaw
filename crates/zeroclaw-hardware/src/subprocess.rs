//! SubprocessTool — wraps any external binary as a [`Tool`].

use super::manifest::ToolManifest;
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use zeroclaw_api::attribution::{ToolKind, ToolProvenance};
use zeroclaw_api::deadline::{Phase, run_inherited_phase};
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_api::tool_attribution;

tool_attribution!(SubprocessTool, ToolKind::Plugin, ToolProvenance::Extension);

/// Total I/O/execution budget; explicit error cleanup has its own bounded wait.
const SUBPROCESS_TIMEOUT_SECS: u64 = 10;

/// Ceiling for natural exit and for explicit kill/reap after an error.
/// The inherited parent deadline can shorten either wait.
const PROCESS_EXIT_TIMEOUT_SECS: u64 = 5;

pub struct SubprocessTool {
    /// Parsed plugin manifest (tool metadata + parameter definitions).
    manifest: ToolManifest,
    /// Resolved absolute path to the entry-point binary.
    binary_path: PathBuf,
}

impl SubprocessTool {
    /// Create a new `SubprocessTool` from a manifest and resolved binary path.
    pub fn new(manifest: ToolManifest, binary_path: PathBuf) -> Self {
        Self {
            manifest,
            binary_path,
        }
    }

    /// Build JSON Schema `properties` and `required` arrays from the manifest.
    fn build_schema_properties(
        &self,
    ) -> (
        serde_json::Map<String, serde_json::Value>,
        Vec<serde_json::Value>,
    ) {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for param in &self.manifest.parameters {
            let mut prop = json!({
                "type": param.r#type,
                "description": param.description,
            });

            if let Some(default) = &param.default {
                prop["default"] = default.clone();
            }

            properties.insert(param.name.clone(), prop);

            if param.required {
                required.push(serde_json::Value::String(param.name.clone()));
            }
        }

        (properties, required)
    }
}

#[async_trait]
impl Tool for SubprocessTool {
    fn name(&self) -> &str {
        &self.manifest.tool.name
    }

    fn description(&self) -> &str {
        &self.manifest.tool.description
    }

    /// JSON Schema Draft 7 — auto-generated from `manifest.parameters`.
    fn parameters_schema(&self) -> serde_json::Value {
        let (properties, required) = self.build_schema_properties();
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        run_inherited_phase(
            Phase::Tool,
            self.execute_with_budget(
                args,
                Duration::from_secs(SUBPROCESS_TIMEOUT_SECS),
                Duration::from_secs(PROCESS_EXIT_TIMEOUT_SECS),
            ),
        )
        .await
    }
}

// Bound the serialized protocol envelope before spawning or allocating an
// unbounded line. The caller's original argument Value remains caller-owned.
const MAX_PROTOCOL_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 512;

// Reserve one byte for the trailing protocol newline.
struct ProtocolBuffer(Vec<u8>);

impl std::io::Write for ProtocolBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_PROTOCOL_BYTES.saturating_sub(self.0.len() + 1) {
            return Err(std::io::Error::other("plugin request exceeds 1 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SubprocessTool {
    async fn execute_with_budget(
        &self,
        args: serde_json::Value,
        operation_budget: Duration,
        cleanup_budget: Duration,
    ) -> anyhow::Result<ToolResult> {
        use anyhow::Context;

        let mut encoded = ProtocolBuffer(Vec::new());
        serde_json::to_writer(&mut encoded, &args)
            .context("failed to encode bounded plugin request before spawn")?;
        let mut child = Command::new(&self.binary_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // The Child is the sole process owner throughout writes, reads and
            // cleanup. Dropping the caller cannot detach a live direct child.
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn plugin '{}'", self.manifest.tool.name))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let mut stderr_prefix = Vec::new();
        // This is the sole received protocol result. Retain its output if a
        // later exit/cleanup check fails; never fabricate execution confirmation.
        let mut received = None;
        let exchange_result = {
            let exchange = async {
                let mut stdin = stdin.context("plugin stdin pipe unavailable")?;
                let write = async {
                    stdin.write_all(&encoded.0).await?;
                    stdin.write_all(b"\n").await
                }
                .await;
                // Fixed-output plugins may close stdin without reading it.
                if let Err(error) = write
                    && error.kind() != std::io::ErrorKind::BrokenPipe
                {
                    return Err(error).context("failed to write plugin arguments");
                }
                drop(stdin);
                let stdout = stdout.context("plugin stdout pipe unavailable")?;
                let mut reader = BufReader::new(stdout).take((MAX_PROTOCOL_BYTES + 1) as u64);
                let mut line = Vec::new();
                reader
                    .read_until(b'\n', &mut line)
                    .await
                    .context("failed to read plugin response")?;
                if line.len() > MAX_PROTOCOL_BYTES {
                    anyhow::bail!("plugin response exceeds 1 MiB");
                }
                if line.iter().all(u8::is_ascii_whitespace) {
                    anyhow::bail!("plugin returned empty stdout");
                }
                received = Some(
                    serde_json::from_slice::<ToolResult>(&line)
                        .context("failed to parse plugin ToolResult")?,
                );
                let status = timeout(cleanup_budget, child.wait())
                    .await
                    .context("plugin exit timed out after its response")?
                    .context("failed to wait for plugin exit")?;
                if !status.success() {
                    anyhow::bail!("plugin exited with {status}");
                }
                Ok::<(), anyhow::Error>(())
            };
            // Poll stderr alongside *all* exchange phases, including a blocked
            // stdin write. Keep only a prefix, but drain excess to avoid pipe
            // backpressure. No spawned drain task can outlive this invocation.
            let drain = drain_stderr(stderr, &mut stderr_prefix);
            timeout(operation_budget, async {
                tokio::pin!(exchange);
                tokio::pin!(drain);
                tokio::select! {
                    result = &mut exchange => result,
                    result = &mut drain => {
                        result.context("failed to drain plugin stderr")?;
                        exchange.await
                    },
                }
            })
            .await
            .map_err(|error| anyhow::Error::new(error).context("plugin exchange timed out"))
            .and_then(std::convert::identity)
        };

        if let Err(error) = exchange_result {
            // Bound explicit reaping too. Parent expiry during this wait drops
            // the same kill-on-drop Child; no detached cleanup task or retry.
            let cleanup = async {
                child.start_kill().context("failed to kill plugin child")?;
                child.wait().await.context("failed to reap plugin child")?;
                Ok::<(), anyhow::Error>(())
            };
            let cleanup_error = match timeout(cleanup_budget, cleanup).await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => Some("plugin child cleanup timed out".to_string()),
            };
            let mut result = received.unwrap_or_else(|| ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: None,
            });
            result.success = false;
            let mut diagnostic = format!(
                "plugin '{}': {error:#}; external effects may have occurred; do not replay automatically",
                self.manifest.tool.name
            );
            if let Some(cleanup_error) = cleanup_error {
                diagnostic.push_str(&format!("; {cleanup_error}"));
            }
            let stderr = String::from_utf8_lossy(&stderr_prefix);
            if !stderr.trim().is_empty() {
                diagnostic.push_str(&format!("; stderr: {}", stderr.trim()));
            }
            if let Some(prior_error) = result.error.take() {
                diagnostic.push_str(&format!("; plugin error: {prior_error}"));
            }
            result.error = Some(diagnostic);
            return Ok(result);
        }
        received.context("plugin exchange completed without a protocol result")
    }
}

async fn drain_stderr(
    handle: Option<tokio::process::ChildStderr>,
    prefix: &mut Vec<u8>,
) -> std::io::Result<()> {
    let Some(mut stderr) = handle else {
        return Ok(());
    };
    let mut buffer = [0u8; 4096];
    loop {
        let n = stderr.read(&mut buffer).await?;
        if n == 0 {
            return Ok(());
        }
        let keep = n.min(MAX_STDERR_BYTES.saturating_sub(prefix.len()));
        prefix.extend_from_slice(&buffer[..keep]);
    }
}

#[cfg(all(test, unix))]
mod lifecycle_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ExecConfig, ParameterDef, ToolManifest, ToolMeta};
    use zeroclaw_api::attribution::Attributable;

    pub(super) fn make_manifest(name: &str, params: Vec<ParameterDef>) -> ToolManifest {
        ToolManifest {
            tool: ToolMeta {
                name: name.to_string(),
                version: "1.0.0".to_string(),
                description: format!("Test tool: {}", name),
            },
            exec: ExecConfig {
                binary: "tool".to_string(),
            },
            transport: None,
            parameters: params,
        }
    }

    fn make_param(name: &str, ty: &str, required: bool) -> ParameterDef {
        ParameterDef {
            name: name.to_string(),
            r#type: ty.to_string(),
            description: format!("param {}", name),
            required,
            default: None,
        }
    }

    #[test]
    fn name_and_description_come_from_manifest() {
        let m = make_manifest("gpio_test", vec![]);
        let tool = SubprocessTool::new(m, PathBuf::from("/bin/true"));
        assert_eq!(tool.name(), "gpio_test");
        assert_eq!(tool.description(), "Test tool: gpio_test");
    }

    #[test]
    fn manifest_loaded_subprocess_is_an_extension() {
        let tool =
            SubprocessTool::new(make_manifest("browser", vec![]), PathBuf::from("/bin/true"));

        assert_eq!(tool.tool_provenance(), ToolProvenance::Extension);
    }

    #[test]
    fn schema_reflects_parameter_definitions() {
        let params = vec![
            make_param("device", "string", true),
            make_param("pin", "integer", true),
            make_param("value", "integer", false),
        ];
        let m = make_manifest("gpio_write", params);
        let tool = SubprocessTool::new(m, PathBuf::from("/bin/true"));
        let schema = tool.parameters_schema();

        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["device"]["type"], "string");
        assert_eq!(schema["properties"]["pin"]["type"], "integer");

        let required = schema["required"].as_array().unwrap();
        let req_names: Vec<&str> = required.iter().map(|v| v.as_str().unwrap()).collect();
        assert!(req_names.contains(&"device"));
        assert!(req_names.contains(&"pin"));
        assert!(!req_names.contains(&"value"));
    }

    #[test]
    fn schema_parameterless_tool_has_empty_required() {
        let m = make_manifest("noop", vec![]);
        let tool = SubprocessTool::new(m, PathBuf::from("/bin/true"));
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.is_empty());
    }

    #[tokio::test]
    async fn execute_successful_subprocess() {
        // Use `echo` to emit a valid ToolResult on stdout.
        // `echo` prints its argument + newline and exits 0.
        let result_json = r#"{"success":true,"output":"ok","error":null}"#;

        // Build a manifest pointing at a tiny protocol helper.
        let m = make_manifest("echo_tool", vec![]);

        let dir = tempfile::tempdir().unwrap();
        let script_path = protocol_helper_path(dir.path());
        std::fs::write(&script_path, protocol_helper_script(result_json)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let tool = SubprocessTool::new(m, script_path.clone());
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .expect("execute should not return Err");

        assert!(result.success, "expected success=true, got: {:?}", result);
        assert_eq!(result.output, "ok");
        assert!(result.error.is_none());
    }

    #[cfg(windows)]
    fn protocol_helper_path(dir: &std::path::Path) -> PathBuf {
        dir.join("tool.cmd")
    }

    #[cfg(not(windows))]
    fn protocol_helper_path(dir: &std::path::Path) -> PathBuf {
        dir.join("tool.sh")
    }

    #[cfg(windows)]
    fn protocol_helper_script(result_json: &str) -> String {
        format!("@echo off\r\nset /p _zc_args=\r\necho {result_json}\r\n")
    }

    #[cfg(not(windows))]
    fn protocol_helper_script(result_json: &str) -> String {
        format!("#!/bin/sh\ncat > /dev/null\necho '{}'\n", result_json)
    }

    #[tokio::test]
    #[ignore = "slow: waits SUBPROCESS_TIMEOUT_SECS (~10 s) to elapse — run manually"]
    async fn execute_timeout_kills_process_and_returns_error() {
        // Script sleeps forever — SubprocessTool should kill it and return a
        // "timed out" error once SUBPROCESS_TIMEOUT_SECS elapses.
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tool.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let m = make_manifest("sleep_tool", vec![]);
        let tool = SubprocessTool::new(m, script_path);
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .expect("should not propagate Err");

        assert!(!result.success);
        let err = result.error.unwrap();
        assert!(
            err.contains("timed out"),
            "expected 'timed out' in error, got: {}",
            err
        );
    }
}
