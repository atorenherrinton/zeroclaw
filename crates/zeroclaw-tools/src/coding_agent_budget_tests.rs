use crate::claude_code::ClaudeCodeTool;
use crate::claude_code_runner::ClaudeCodeRunnerTool;
use crate::codex_cli::CodexCliTool;
use crate::coding_cli::{CodingCliCommand, CodingCliExecutionError, CodingCliExecutor};
use crate::gemini_cli::GeminiCliTool;
use crate::opencode_cli::OpenCodeCliTool;
use crate::wrappers::RateLimitedTool;
use async_trait::async_trait;
use serde_json::json;
use std::path::Path;
use std::process::{ExitStatus, Output};
use std::sync::Arc;
use zeroclaw_api::tool::Tool;
use zeroclaw_config::autonomy::AutonomyLevel;
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{
    ClaudeCodeConfig, ClaudeCodeRunnerConfig, CodexCliConfig, GeminiCliConfig, OpenCodeCliConfig,
};

// Codex uses `exec`; the other mocked coding CLIs use different subcommands.
// Its configured test binary must reach the mock, never a PATH-resolved CLI.
fn assert_test_local_codex_executable(command: &CodingCliCommand) {
    if command.args.first().is_some_and(|arg| arg == "exec") {
        assert_eq!(
            Path::new(&command.program),
            std::env::current_exe()
                .expect("test executable")
                .canonicalize()
                .expect("canonical test executable")
        );
    }
}

#[derive(Debug)]
struct SuccessfulExecutor;

#[async_trait]
impl CodingCliExecutor for SuccessfulExecutor {
    async fn output(&self, command: CodingCliCommand) -> Result<Output, CodingCliExecutionError> {
        assert_test_local_codex_executable(&command);
        Ok(Output {
            status: successful_exit_status(),
            stdout: b"ok".to_vec(),
            stderr: Vec::new(),
        })
    }
}

#[cfg(unix)]
fn successful_exit_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn successful_exit_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

fn policy(
    autonomy: AutonomyLevel,
    max_actions_per_hour: u32,
    workspace: &Path,
) -> Arc<SecurityPolicy> {
    Arc::new(SecurityPolicy {
        autonomy,
        max_actions_per_hour,
        workspace_dir: workspace.to_path_buf(),
        ..SecurityPolicy::default()
    })
}

fn wrapped_tool<T: Tool + 'static>(inner: T, security: Arc<SecurityPolicy>) -> Box<dyn Tool> {
    Box::new(RateLimitedTool::new(inner, security))
}

#[cfg(unix)]
fn write_successful_tmux(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, "#!/bin/sh\nexit 0\n").expect("write tmux fixture");
    let mut permissions = std::fs::metadata(path)
        .expect("tmux fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).expect("make tmux fixture executable");
}

#[cfg(unix)]
fn coding_agent_cases(
    autonomy: AutonomyLevel,
    max_actions_per_hour: u32,
    workspace: &Path,
    tmux_binary: &Path,
) -> Vec<(Arc<SecurityPolicy>, Box<dyn Tool>)> {
    let executor: Arc<dyn CodingCliExecutor> = Arc::new(SuccessfulExecutor);
    let mut cases = Vec::new();

    let security = policy(autonomy, max_actions_per_hour, workspace);
    cases.push((
        security.clone(),
        wrapped_tool(
            ClaudeCodeTool::new_with_executor(
                security.clone(),
                ClaudeCodeConfig::default(),
                executor.clone(),
            ),
            security,
        ),
    ));

    let security = policy(autonomy, max_actions_per_hour, workspace);
    cases.push((
        security.clone(),
        wrapped_tool(
            ClaudeCodeRunnerTool::new(
                security.clone(),
                ClaudeCodeRunnerConfig::default(),
                "http://localhost:3000".into(),
            )
            .with_tmux_binary(tmux_binary.to_path_buf()),
            security,
        ),
    ));

    let security = policy(autonomy, max_actions_per_hour, workspace);
    cases.push((
        security.clone(),
        wrapped_tool(
            CodexCliTool::new_with_executor(
                security.clone(),
                CodexCliConfig {
                    executable_path: Some(std::env::current_exe().expect("test executable")),
                    recovery_source_workspace: Some(workspace.to_path_buf()),
                    ..CodexCliConfig::default()
                },
                executor.clone(),
            ),
            security,
        ),
    ));

    let security = policy(autonomy, max_actions_per_hour, workspace);
    cases.push((
        security.clone(),
        wrapped_tool(
            GeminiCliTool::new_with_executor(
                security.clone(),
                GeminiCliConfig::default(),
                executor.clone(),
            ),
            security,
        ),
    ));

    let security = policy(autonomy, max_actions_per_hour, workspace);
    cases.push((
        security.clone(),
        wrapped_tool(
            OpenCodeCliTool::new_with_executor(
                security.clone(),
                OpenCodeCliConfig::default(),
                executor,
            ),
            security,
        ),
    ));

    cases
}

#[cfg(unix)]
#[tokio::test]
async fn coding_agent_wrappers_charge_one_action_each() {
    let workspace = tempfile::TempDir::new().expect("workspace");
    mark_as_zeroclaw_source(workspace.path());
    let tmux_binary = workspace.path().join("tmux");
    write_successful_tmux(&tmux_binary);

    for (security, tool) in
        coding_agent_cases(AutonomyLevel::Full, 2, workspace.path(), &tmux_binary)
    {
        let tool_name = tool.name().to_string();
        let invocation = tool.execute(json!({"prompt": "hello"}));
        let result = if tool_name == "codex_cli" {
            crate::codex_cli::scope_zeroclaw_recovery("repair ZeroClaw".into(), invocation).await
        } else {
            invocation.await
        }
        .expect("coding-agent invocation");
        assert!(result.success, "{tool_name} should succeed: {result:?}");
        assert!(
            security.record_action(),
            "{tool_name} should leave exactly one of two action slots available"
        );
        assert!(
            !security.record_action(),
            "{tool_name} must not leave a third action slot"
        );
    }
}

#[cfg(unix)]
fn mark_as_zeroclaw_source(workspace: &std::path::Path) {
    std::fs::create_dir_all(workspace.join("crates/zeroclaw-runtime"))
        .expect("runtime marker directory");
    std::fs::create_dir_all(workspace.join("crates/zeroclaw-tools"))
        .expect("tools marker directory");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"crates/zeroclaw-runtime\", \"crates/zeroclaw-tools\"]\n\n[package]\nname = \"zeroclaw\"\nversion = \"0.0.0\"\n",
    )
    .expect("root source manifest");
    std::fs::write(
        workspace.join("crates/zeroclaw-runtime/Cargo.toml"),
        "[package]\nname = \"zeroclaw-runtime\"\nversion = \"0.0.0\"\n",
    )
    .expect("runtime source manifest");
    std::fs::write(
        workspace.join("crates/zeroclaw-tools/Cargo.toml"),
        "[package]\nname = \"zeroclaw-tools\"\nversion = \"0.0.0\"\n",
    )
    .expect("tools source manifest");
}

#[cfg(unix)]
#[tokio::test]
async fn coding_agent_readonly_rejection_consumes_no_action() {
    let workspace = tempfile::TempDir::new().expect("workspace");
    let tmux_binary = workspace.path().join("tmux");

    for (security, tool) in
        coding_agent_cases(AutonomyLevel::ReadOnly, 1, workspace.path(), &tmux_binary)
    {
        let tool_name = tool.name().to_string();
        let invocation = tool.execute(json!({"prompt": "hello"}));
        let result = if tool_name == "codex_cli" {
            crate::codex_cli::scope_zeroclaw_recovery("repair ZeroClaw".into(), invocation).await
        } else {
            invocation.await
        }
        .expect("read-only rejection");
        assert!(
            !result.success,
            "{tool_name} must be rejected in read-only mode"
        );
        assert!(
            result.error.as_deref().unwrap_or("").contains("read-only"),
            "{tool_name} should report the autonomy rejection"
        );
        assert!(
            security.record_action(),
            "{tool_name} rejection must leave the action slot available"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn completed_coding_commands_bound_both_streams_without_reexecution() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct LargeExecutor {
        calls: AtomicUsize,
        exit_code: i32,
        as_json: bool,
    }
    #[async_trait]
    impl CodingCliExecutor for LargeExecutor {
        async fn output(
            &self,
            command: CodingCliCommand,
        ) -> Result<Output, CodingCliExecutionError> {
            use std::os::unix::process::ExitStatusExt;
            assert_test_local_codex_executable(&command);
            self.calls.fetch_add(1, Ordering::SeqCst);
            let text = format!("START{}END", "\\\"\t😀".repeat(20_000));
            let stdout = if self.as_json {
                json!({"result":text,"session_id":"fixture-session"}).to_string()
            } else {
                text
            };
            Ok(Output {
                status: ExitStatus::from_raw(self.exit_code << 8),
                stdout: stdout.into_bytes(),
                stderr: format!("ERROR{}TAIL", "\\\"\t😀".repeat(20_000)).into_bytes(),
            })
        }
    }
    for (exit_code, as_json) in [(0, false), (7, false), (0, true), (7, true)] {
        let workspace = tempfile::TempDir::new().unwrap();
        mark_as_zeroclaw_source(workspace.path());
        let security = policy(AutonomyLevel::Full, 100, workspace.path());
        let executor = Arc::new(LargeExecutor {
            calls: AtomicUsize::new(0),
            exit_code,
            as_json,
        });
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(ClaudeCodeTool::new_with_executor(
                security.clone(),
                ClaudeCodeConfig::default(),
                executor.clone(),
            )),
            Box::new(CodexCliTool::new_with_executor(
                security.clone(),
                CodexCliConfig {
                    executable_path: Some(std::env::current_exe().expect("test executable")),
                    recovery_source_workspace: Some(workspace.path().to_path_buf()),
                    ..Default::default()
                },
                executor.clone(),
            )),
            Box::new(GeminiCliTool::new_with_executor(
                security.clone(),
                GeminiCliConfig::default(),
                executor.clone(),
            )),
            Box::new(OpenCodeCliTool::new_with_executor(
                security,
                OpenCodeCliConfig::default(),
                executor.clone(),
            )),
        ];
        for tool in tools {
            let invocation = tool.execute(json!({"prompt":"fixture only"}));
            let result = if tool.name() == "codex_cli" {
                crate::codex_cli::scope_zeroclaw_recovery("fixture recovery".into(), invocation)
                    .await
            } else {
                invocation.await
            }
            .unwrap();
            assert_eq!(
                result.success,
                exit_code == 0,
                "{}: {:?}",
                tool.name(),
                result.error
            );
            assert!(result.output.contains("START"));
            assert!(result.output.contains("END"));
            if as_json {
                assert!(result.output.contains("fixture-session"));
            }
            assert!(result.output.contains("command already ran"));
            assert!(crate::output_budget::encoded_size(result.output.as_str(), 2048).is_some());
            let error = result.error.as_deref().unwrap();
            assert!(error.starts_with("ERROR") && error.ends_with("TAIL"));
            assert!(crate::output_budget::encoded_size(error, 2048).is_some());
        }
        assert_eq!(executor.calls.load(Ordering::SeqCst), 4);
    }
}
