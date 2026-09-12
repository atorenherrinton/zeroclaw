use crate::cron::{self, JobType};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::runtime_traits::RuntimeAdapter;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::schema::Config;

pub struct CronRunTool {
    config: Arc<Config>,
    security: Arc<SecurityPolicy>,
    /// Owning agent — another agent's job cannot be triggered from here.
    agent_alias: String,
    runtime: Arc<dyn RuntimeAdapter>,
}

impl CronRunTool {
    pub fn new_with_runtime(
        config: Arc<Config>,
        security: Arc<SecurityPolicy>,
        agent_alias: impl Into<String>,
        runtime: Arc<dyn RuntimeAdapter>,
    ) -> Self {
        Self {
            config,
            security,
            agent_alias: agent_alias.into(),
            runtime,
        }
    }

    #[cfg(test)]
    pub fn new(
        config: Arc<Config>,
        security: Arc<SecurityPolicy>,
        agent_alias: impl Into<String>,
    ) -> Self {
        let runtime = Arc::from(
            crate::platform::create_runtime(&config.runtime)
                .expect("test config must construct its runtime"),
        );
        Self::new_with_runtime(config, security, agent_alias, runtime)
    }
}

#[async_trait]
impl Tool for CronRunTool {
    fn name(&self) -> &str {
        "cron_run"
    }

    fn description(&self) -> &str {
        "Force-run a cron job immediately and record run history"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "job_id": { "type": "string" },
                "request_id": { "type": "string", "minLength": 1, "maxLength": 128,
                    "description": crate::i18n::get_required_cli_string("cron-manual-request-id-description") },
                "approved": {
                    "type": "boolean",
                    "description": "Set true to explicitly approve medium/high-risk shell commands in supervised mode",
                    "default": false
                }
            },
            "required": ["job_id"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if !self.config.scheduler.enabled {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("cron is disabled by config (scheduler.enabled=false)".to_string()),
            });
        }

        let job_id = match args.get("job_id").and_then(serde_json::Value::as_str) {
            Some(v) if !v.trim().is_empty() => v,
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing 'job_id' parameter".to_string()),
                });
            }
        };
        let request_id = match args.get("request_id") {
            None => None,
            Some(serde_json::Value::String(value)) => Some(value.as_str()),
            Some(_) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(crate::i18n::get_required_cli_string(
                        "cron-manual-invalid-request-id",
                    )),
                });
            }
        };
        let approved = args
            .get("approved")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Security policy: read-only mode, cannot perform 'cron_run'".into()),
            });
        }

        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        let job = match cron::get_job_for_agent(&self.config, job_id, &self.agent_alias) {
            Ok(job) => job,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(e.to_string()),
                });
            }
        };

        if matches!(job.job_type, JobType::Shell)
            && let Err(reason) = cron::validate_shell_command_with_security(
                self.runtime.as_ref(),
                &self.security,
                &job.command,
                approved,
            )
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(reason.to_string()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        let result = cron::scheduler::run_manual_job_with_runtime(
            &self.config,
            &job,
            cron::scheduler::CronDeliveryContext::ToolManual,
            &None,
            self.runtime.as_ref(),
            approved,
            request_id,
        )
        .await;

        Ok(ToolResult {
            success: result.success,
            output: serde_json::to_string_pretty(&json!({
                "duplicate": result.duplicate,
                "occurrence_id": result.occurrence_id,
                "effect_outcome": result.effect_outcome,
                "execution_outcome": result.execution_outcome,
                "delivery_outcome": result.delivery_outcome,
                "job_id": result.job_id,
                "status": result.status,
                "duration_ms": result.duration_ms,
                "output": result.output
            }))?
            .into(),
            error: if result.success {
                None
            } else {
                Some("cron job execution failed".to_string())
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::AutonomyLevel;
    use tempfile::TempDir;
    use zeroclaw_config::schema::Config;

    const TEST_AGENT: &str = "test-agent";

    async fn test_config(tmp: &TempDir) -> Arc<Config> {
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        seed_test_agent(&mut config);
        tokio::fs::create_dir_all(&config.data_dir).await.unwrap();
        Arc::new(config)
    }

    fn seed_test_agent(config: &mut Config) {
        config
            .risk_profiles
            .entry(TEST_AGENT.to_string())
            .or_default();
        config
            .runtime_profiles
            .entry(TEST_AGENT.to_string())
            .or_default();
        config
            .providers
            .models
            .ensure("openrouter", TEST_AGENT)
            .expect("known family");
        config.agents.entry(TEST_AGENT.to_string()).or_insert(
            zeroclaw_config::schema::AliasedAgentConfig {
                model_provider: format!("openrouter.{TEST_AGENT}").into(),
                risk_profile: TEST_AGENT.into(),
                runtime_profile: TEST_AGENT.into(),
                ..Default::default()
            },
        );
    }

    fn test_security(cfg: &Config) -> Arc<SecurityPolicy> {
        Arc::new(
            SecurityPolicy::for_agent(cfg, TEST_AGENT).expect("test-agent has resolvable profiles"),
        )
    }

    #[tokio::test]
    async fn isolated_agent_cron_does_not_borrow_parent_journal_or_replay_tools() {
        use crate::control_plane::{
            SqliteTaskStore, TaskKind, TaskRecord, TaskRegistry, TaskStatus,
        };
        use axum::{Json, Router, routing::post};
        use zeroclaw_api::turn::{JOURNAL, TurnJournal};
        use zeroclaw_config::schema::{ModelProviderConfig, OllamaModelProviderConfig};

        struct ParentJournal(Arc<SqliteTaskStore>);
        #[async_trait::async_trait]
        impl TurnJournal for ParentJournal {
            async fn checkpoint(
                &self,
                status: TaskStatus,
                output: Option<String>,
                delivered: bool,
            ) -> anyhow::Result<()> {
                self.0
                    .checkpoint_channel_turn("parent", status, output, delivered)
                    .await
            }
        }
        let store = Arc::new(SqliteTaskStore::new_in_memory().unwrap());
        store
            .create(TaskRecord {
                id: "parent".into(),
                kind: TaskKind::ChannelTurn,
                agent: TEST_AGENT.into(),
                status: TaskStatus::Running,
                owner_pid: std::process::id(),
                owner_boot_id: "fixture".into(),
                heartbeat_at: None,
                depth: 0,
                parent_id: None,
                originator_route: None,
                delivered: false,
                idem_key: None,
                principal_id: None,
                started_at: chrono::Utc::now().to_rfc3339(),
                finished_at: None,
            })
            .await
            .unwrap();
        store
            .checkpoint_channel_turn("parent", TaskStatus::WaitingOnTool, None, false)
            .await
            .unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let captured = requests.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<serde_json::Value>| {
                captured.lock().unwrap().push(body.clone());
                async move {
                    let results = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count();
                    let message = if results == 2 {
                        json!({"content":"done"})
                    } else {
                        assert!(results < 2);
                        json!({"content":null,"tool_calls":[{
                            "id":format!("call-{results}"),"type":"function",
                            "function":{"name":"shell","arguments":json!({
                                "command":format!("echo step-{results}")
                            }).to_string()}
                        }]})
                    };
                    Json(json!({"choices":[{"message":message}]}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tmp = TempDir::new().unwrap();
        let mut config = (*test_config(&tmp).await).clone();
        config.memory.backend = "none".into();
        config.memory.auto_save = false;
        config.reliability.scheduler_retries = 2;
        config.providers.models.ollama.insert(
            "default".into(),
            OllamaModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("cron-fixture".into()),
                    timeout_secs: Some(5),
                    uri: Some(format!("http://{address}")),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        config.agents.get_mut(TEST_AGENT).unwrap().model_provider = "ollama.default".into();
        config.risk_profiles.get_mut(TEST_AGENT).unwrap().level = AutonomyLevel::Full;
        let job = cron::add_agent_job(
            &config,
            TEST_AGENT,
            None,
            cron::Schedule::Cron {
                expr: "0 * * * *".into(),
                tz: None,
            },
            "Run the two fixture tools and return their results",
            cron::SessionTarget::Isolated,
            None,
            None,
            false,
            Some(vec!["shell".into()]),
            false,
        )
        .unwrap();
        config
            .agents
            .get_mut(TEST_AGENT)
            .unwrap()
            .cron_jobs
            .push(job.id.clone());
        let cfg = Arc::new(config);
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);
        let args = json!({"job_id":job.id,"request_id":"isolated-two-tools"});
        let result = JOURNAL
            .scope(
                Some(Arc::new(ParentJournal(store.clone()))),
                tool.execute(args.clone()),
            )
            .await
            .unwrap();
        assert!(result.success, "{} {:?}", result.output, result.error);
        assert_eq!(
            store.get("parent").await.unwrap().unwrap().status,
            TaskStatus::WaitingOnTool
        );
        store
            .checkpoint_channel_turn("parent", TaskStatus::Running, None, false)
            .await
            .unwrap();
        let duplicate = tool.execute(args).await.unwrap();
        assert!(duplicate.success);
        let duplicate: serde_json::Value = serde_json::from_str(&duplicate.output).unwrap();
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(
            requests.lock().unwrap().len(),
            3,
            "retry must not invoke the provider or tools again"
        );
        let requests = requests.lock().unwrap();
        let outputs: Vec<_> = requests[2]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[0]["content"].to_string().contains("step-0"));
        assert!(outputs[1]["content"].to_string().contains("step-1"));
        assert_eq!(cron::list_runs(&cfg, &job.id, 10).unwrap().len(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn force_runs_job_and_records_history() {
        let tmp = TempDir::new().unwrap();
        // Build the config so we can wire the imperative job's UUID
        // into test-agent's cron_jobs list before wrapping in Arc —
        // otherwise execute_job_now's reverse-lookup can't find the
        // owning agent.
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        seed_test_agent(&mut config);
        tokio::fs::create_dir_all(&config.data_dir).await.unwrap();
        let job = cron::add_job(&config, TEST_AGENT, "*/5 * * * *", "echo run-now").unwrap();
        config
            .agents
            .get_mut(TEST_AGENT)
            .unwrap()
            .cron_jobs
            .push(job.id.clone());
        let cfg = Arc::new(config);
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        let result = tool
            .execute(json!({ "job_id": job.id, "request_id":"tool-fixture-request" }))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);

        let original: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        let duplicate = tool
            .execute(json!({"job_id":job.id,"request_id":"tool-fixture-request"}))
            .await
            .unwrap();
        let duplicate: serde_json::Value = serde_json::from_str(&duplicate.output).unwrap();
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["occurrence_id"], original["occurrence_id"]);
        assert_eq!(duplicate["effect_outcome"], "confirmed");
        let runs = cron::list_runs(&cfg, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
    }

    #[tokio::test]
    async fn best_effort_delivery_failure_records_degraded_history() {
        cron::scheduler::register_delivery_fn(Box::new(
            |_config, channel, _target, _thread_id, _output| {
                Box::pin(async move {
                    if channel == "fail-delivery" {
                        anyhow::bail!("synthetic delivery failure");
                    }
                    Ok(())
                })
            },
        ));

        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        seed_test_agent(&mut config);
        tokio::fs::create_dir_all(&config.data_dir).await.unwrap();
        let job = cron::add_shell_job_with_approval(
            &config,
            TEST_AGENT,
            None,
            cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo run-now",
            Some(cron::DeliveryConfig {
                mode: "announce".into(),
                channel: Some("fail-delivery".into()),
                to: Some("123456".into()),
                thread_id: None,
                reply_to: None,
                best_effort: true,
            }),
            true,
        )
        .unwrap();
        config
            .agents
            .get_mut(TEST_AGENT)
            .unwrap()
            .cron_jobs
            .push(job.id.clone());
        let cfg = Arc::new(config);
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(result.success, "{:?}", result.error);
        let response: serde_json::Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(response["status"], "degraded");
        assert!(
            response["output"]
                .as_str()
                .unwrap_or_default()
                .contains("delivery failed:")
        );

        let updated = cron::get_job(&cfg, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("uncertain"));
        assert!(!updated.enabled);
        assert_eq!(response["effect_outcome"], "reconciliation_required");
        assert_eq!(response["execution_outcome"], "confirmed");
        assert!(
            updated
                .last_output
                .as_deref()
                .unwrap_or_default()
                .contains("delivery failed:")
        );

        let runs = cron::list_runs(&cfg, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "degraded");
        assert!(
            runs[0]
                .output
                .as_deref()
                .unwrap_or_default()
                .contains("delivery failed:")
        );
    }

    #[tokio::test]
    async fn errors_for_missing_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        let result = tool
            .execute(json!({ "job_id": "missing-job-id" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("not found"));
    }

    #[tokio::test]
    async fn blocks_run_in_read_only_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.data_dir).unwrap();
        seed_test_agent(&mut config);
        let job = cron::add_job(&config, TEST_AGENT, "*/5 * * * *", "echo run-now").unwrap();
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .level = AutonomyLevel::ReadOnly;
        let cfg = Arc::new(config);
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap_or_default().contains("read-only"));
    }

    #[tokio::test]
    async fn shell_run_requires_approval_for_medium_risk() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        seed_test_agent(&mut config);
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .level = AutonomyLevel::Supervised;
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .allowed_commands = vec!["touch".into()];
        std::fs::create_dir_all(&config.data_dir).unwrap();
        seed_test_agent(&mut config);
        let cfg = Arc::new(config);
        // Create with explicit approval so the job persists for the run test.
        let job = cron::add_shell_job_with_approval(
            &cfg,
            TEST_AGENT,
            None,
            cron::Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "touch cron-run-approval",
            None,
            true,
        )
        .unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        // Without approval, the tool-level policy check blocks medium-risk commands.
        let denied = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!denied.success);
        assert!(
            denied
                .error
                .unwrap_or_default()
                .contains("explicit approval")
        );
    }

    #[tokio::test]
    async fn blocks_run_when_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = Config {
            data_dir: tmp.path().join("data"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        seed_test_agent(&mut config);
        config
            .risk_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .level = AutonomyLevel::Full;
        config
            .runtime_profiles
            .entry(TEST_AGENT.into())
            .or_default()
            .max_actions_per_hour = 0;
        std::fs::create_dir_all(&config.data_dir).unwrap();
        seed_test_agent(&mut config);
        let cfg = Arc::new(config);
        let job = cron::add_job(&cfg, TEST_AGENT, "*/5 * * * *", "echo run-now").unwrap();
        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);

        let result = tool.execute(json!({ "job_id": job.id })).await.unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .unwrap_or_default()
                .contains("Rate limit exceeded")
        );
        assert!(cron::list_runs(&cfg, &job.id, 10).unwrap().is_empty());
    }

    /// A job owned by someone else. An agent job needs no risk profile for its
    /// owner, which keeps the fixture to the ownership boundary.
    fn other_agents_job(cfg: &Config) -> crate::cron::CronJob {
        cron::add_agent_job(
            cfg,
            "other-agent",
            Some("secret_job".into()),
            crate::cron::Schedule::Cron {
                expr: "0 8 * * *".into(),
                tz: None,
            },
            "read the other agent's inbox",
            crate::cron::SessionTarget::Isolated,
            None,
            None,
            false,
            None,
            true,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn cannot_trigger_another_agents_job() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_config(&tmp).await;
        let theirs = other_agents_job(&cfg);

        let tool = CronRunTool::new(cfg.clone(), test_security(&cfg), TEST_AGENT);
        let result = tool.execute(json!({"job_id": theirs.id})).await.unwrap();

        assert!(!result.success);
        assert!(
            cron::list_runs(&cfg, &theirs.id, 10).unwrap().is_empty(),
            "another agent's job must not have been executed"
        );
    }
}
