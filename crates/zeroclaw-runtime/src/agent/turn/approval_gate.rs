//! The per-tool-call approval gate: CLI prompt, channel inline approval, or
//! auto-deny, plus decision recording.

use super::context::TurnCtx;
use super::events::StreamDelta;
use super::redact::scrub_credentials;
use crate::agent::tool_execution::{ToolExecutionOutcome, ToolFailureKind};
use crate::approval::{ApprovalRequest, ApprovalRequirement, ApprovalResponse};
use std::time::Duration;

pub(crate) enum ApprovalGateOutcome {
    Proceed { approved: bool },
    Deny(ToolExecutionOutcome),
    Replace(ToolExecutionOutcome),
}

/// Run the approval flow for one tool call (upstream loop body, approval
/// section): resolve the tool's approval requirement, prompt interactively on
/// CLI or via the channel's inline approval on non-interactive channels
/// (falling back to auto-deny), and record the decision.
pub(crate) async fn gate_tool_approval(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    tool_args: &serde_json::Value,
    iteration: usize,
) -> anyhow::Result<ApprovalGateOutcome> {
    let mut approval_requirement = ctx
        .approval
        .map(|mgr| mgr.approval_requirement(tool_name))
        .unwrap_or(ApprovalRequirement::NotRequired);
    if let Some(mgr) = ctx.approval
        && approval_requirement == ApprovalRequirement::Prompt
    {
        let request = ApprovalRequest {
            tool_name: tool_name.to_string(),
            arguments: tool_args.clone(),
        };

        // Interactive CLI: prompt the operator.
        // Non-interactive (channels): try the channel's inline
        // approval (e.g. Telegram inline keyboard) before falling
        // back to auto-deny.
        let (decision, decided_by, unanswerable) = if mgr.is_non_interactive() {
            let attributed = if let Some(ch) = ctx.channel {
                let ch_request = zeroclaw_api::channel::ChannelApprovalRequest {
                    tool_name: request.tool_name.clone(),
                    arguments_summary: crate::approval::summarize_args(&request.arguments),
                    raw_arguments: Some(request.arguments.clone()),
                };
                let recipient = ctx.channel_reply_target.unwrap_or_default();
                match await_approval(
                    ctx,
                    tool_name,
                    ch.request_approval_attributed(recipient, &ch_request),
                )
                .await
                {
                    Ok(Some(a)) => Some(a),
                    Ok(None) => None,
                    Err(e) if super::outcome::is_tool_loop_cancelled(&e) => return Err(e),
                    Err(e) => {
                        ::zeroclaw_log::record!(
                            WARN,
                            ::zeroclaw_log::Event::new(
                                module_path!(),
                                ::zeroclaw_log::Action::Fail
                            )
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                            .with_attrs(::serde_json::json!({"error": format!("{}", e)})),
                            "Channel approval request failed"
                        );
                        None
                    }
                }
            } else {
                None
            };
            // The deciding back-channel (when a fan-out bridge answered) rides
            // back on the response itself, so attribution can't be cross-wired
            // by a concurrent approval on the same channel instance.
            let decided_by = attributed.as_ref().and_then(|a| a.decided_by.clone());
            // Whether an operator actually decided, taken from the response's own
            // provenance rather than inferred.
            //
            // `attributed.is_none()` is NOT sufficient: a fail-closed approval route
            // returns `Some(Deny)` with no decider when the approver is missing,
            // unreachable, silent, or timed out, and a direct channel timeout does the
            // same. Those are runtime denials wearing an operator's clothes. Nor does
            // `decided_by.is_none()` work, since a single non-fan-out channel leaves
            // that `None` for a real human answer.
            let unanswerable = attributed
                .as_ref()
                .map(|a| a.source.is_runtime_fail_closed())
                .unwrap_or(true);
            let decision = match attributed.map(|a| a.response) {
                Some(zeroclaw_api::channel::ChannelApprovalResponse::Approve) => {
                    ApprovalResponse::Yes
                }
                Some(zeroclaw_api::channel::ChannelApprovalResponse::AlwaysApprove) => {
                    ApprovalResponse::Always
                }
                Some(zeroclaw_api::channel::ChannelApprovalResponse::Deny) => ApprovalResponse::No,
                Some(zeroclaw_api::channel::ChannelApprovalResponse::DenyWithEdit {
                    replacement,
                }) => ApprovalResponse::ReplaceWith(replacement),
                // Channel doesn't support approval — auto-deny.
                None => ApprovalResponse::No,
            };
            (decision, decided_by, unanswerable)
        } else {
            // Input failures are unavailable routes, never a claimed operator
            // refusal. Cancellation escapes before any decision is recorded.
            match await_approval(ctx, tool_name, async {
                Ok(mgr.prompt_cli_async(&request).await)
            })
            .await?
            {
                Ok(response) => (response, None, false),
                Err(error) => {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                            .with_category(::zeroclaw_log::EventCategory::Tool)
                            .with_attrs(::serde_json::json!({"error": error.to_string()})),
                        "CLI approval input unavailable"
                    );
                    (ApprovalResponse::No, None, true)
                }
            }
        };

        let decision_channel = decided_by.unwrap_or_else(|| ctx.channel_name.to_string());
        mgr.record_decision(tool_name, tool_args, &decision, &decision_channel);

        if decision == ApprovalResponse::No {
            // This string is fed back to the MODEL, so it states the outcome and
            // stops there. It deliberately does not name the settings that would
            // permit the call: `auto_approve` bypasses operator approval for that
            // tool and `level = "full"` removes approval gates for every tool and
            // drops workspace-only confinement. Putting that remedy in front of the
            // model invites it to argue for expanding its own privileges, which is a
            // disproportionate response to an approval channel being unavailable.
            // Operators get the actionable advice through the WARN record below and
            // the UI, where changing policy is actually their decision to make.
            let denied = if unanswerable {
                format!(
                    "Tool call not executed: '{tool_name}' requires approval and no operator \
                     decision was available, so the runtime denied it by policy. This was not \
                     a user's decision."
                )
            } else {
                // A real operator said no. The three-word form this replaces
                // carried the fact and none of its meaning, so the model
                // supplied the meaning itself and did not do it the same way
                // twice: on one run it reported the decline correctly, on the
                // next it offered three invented causes, none of them what
                // happened. The host owns the fact, so the host states what it
                // means. `Denied by user.` is kept as the opening sentence
                // because it is the phrase that distinguishes this path from
                // the runtime-generated denial above, and dropping it would
                // lose that distinction for every reader that already looks
                // for it.
                format!(
                    "Denied by user. The operator was asked to approve \
                     '{tool_name}' and declined, so the call did not run. Tell \
                     the user the request was declined. Do not retry this call \
                     and do not speculate about why it was declined."
                )
            };
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                    .with_attrs(::serde_json::json!({
                        "model": ctx.model,
                        "iteration": iteration + 1,
                        "tool": tool_name,
                        "arguments": scrub_credentials(&tool_args.to_string()),
                        "result": denied,
                        "trace_id": ctx.turn_id,
                        // Operator-facing only. The remedy lives here rather than
                        // in `result`, which is shown to the model: deciding to
                        // relax an approval policy is the operator's call, and
                        // putting the option in front of the model would invite it
                        // to lobby for its own privilege expansion.
                        "denied_by_runtime": unanswerable,
                        "operator_hint": if unanswerable {
                            Some("No operator could be asked. Check that an approval-capable \
                                  channel is connected and that the agent's approval route names \
                                  a registered, reachable approver. If this tool should run \
                                  unattended, review the agent's risk profile deliberately.")
                        } else {
                            None
                        },
                    })),
                "tool_call_result"
            );
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{274c} {}: {}\n",
                        tool_name, denied
                    )))
                    .await;
            }
            return Ok(ApprovalGateOutcome::Deny(ToolExecutionOutcome {
                output: denied.clone(),
                success: false,
                error_reason: Some(denied),
                failure_kind: Some(if unanswerable {
                    ToolFailureKind::PolicyDenied
                } else {
                    ToolFailureKind::OperatorDenied
                }),
                duration: Duration::ZERO,
                receipt: None,
                output_data: None,
            }));
        }

        if let ApprovalResponse::ReplaceWith(replacement) = &decision {
            if let Some(tx) = ctx.on_delta {
                let _ = tx
                    .send(StreamDelta::Status(format!(
                        "\u{270f} {}: replaced by user\n",
                        tool_name
                    )))
                    .await;
            }
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Approve)
                    .with_category(::zeroclaw_log::EventCategory::Tool)
                    .with_outcome(::zeroclaw_log::EventOutcome::Success)
                    .with_attrs(::serde_json::json!({
                        "model": ctx.model,
                        "iteration": iteration + 1,
                        "tool": tool_name,
                        "arguments": scrub_credentials(&tool_args.to_string()),
                        "replaced": true,
                        "output": scrub_credentials(replacement),
                        "trace_id": ctx.turn_id,
                    })),
                "tool_call_result"
            );
            return Ok(ApprovalGateOutcome::Replace(ToolExecutionOutcome {
                output: crate::approval::sanitize_tool_replacement(replacement),
                success: true,
                error_reason: None,
                failure_kind: None,
                duration: Duration::ZERO,
                receipt: None,
                output_data: None,
            }));
        }

        if matches!(decision, ApprovalResponse::Yes | ApprovalResponse::Always) {
            approval_requirement = ApprovalRequirement::Approved;
        }
    }

    Ok(ApprovalGateOutcome::Proceed {
        approved: approval_requirement == ApprovalRequirement::Approved,
    })
}

/// A cancelled wait produces no approval decision or allowlist mutation. The
/// turn's normal cancellation path retains the terminal cause and history.
async fn await_approval<T>(
    ctx: &TurnCtx<'_>,
    tool_name: &str,
    future: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    let estop = crate::security::estop_runtime::current();
    let wait = async {
        if let Some(runtime) = &estop {
            runtime.run(Some(tool_name), future).await
        } else {
            future.await
        }
    };
    let response = super::outcome::until_cancelled(ctx.cancellation_token, wait).await??;
    if let Some(runtime) = estop {
        runtime.check(Some(tool_name))?;
    }
    if ctx
        .cancellation_token
        .is_some_and(|token| token.is_cancelled())
    {
        return Err(super::outcome::ToolLoopCancelled.into());
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalManager;
    use crate::observability::NoopObserver;
    use crate::security::estop::{EstopLevel, EstopManager};
    use crate::security::estop_runtime::{self, EstopInterrupted, EstopRuntime};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::{Notify, oneshot};
    use tokio_util::sync::CancellationToken;
    use zeroclaw_api::channel::{AttributedApprovalResponse, Channel, ChannelApprovalResponse};
    use zeroclaw_config::schema::{PacingConfig, RiskProfileConfig, StreamReasoningMode};

    struct WaitingChannel {
        ready: Notify,
        dropped: AtomicBool,
        response:
            parking_lot::Mutex<Option<oneshot::Receiver<anyhow::Result<ChannelApprovalResponse>>>>,
    }

    impl WaitingChannel {
        fn new() -> (
            Self,
            oneshot::Sender<anyhow::Result<ChannelApprovalResponse>>,
        ) {
            let (tx, rx) = oneshot::channel();
            (
                Self {
                    ready: Notify::new(),
                    dropped: AtomicBool::new(false),
                    response: parking_lot::Mutex::new(Some(rx)),
                },
                tx,
            )
        }
    }

    impl zeroclaw_api::attribution::Attributable for WaitingChannel {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Channel(
                zeroclaw_api::attribution::ChannelKind::AcpChannel,
            )
        }
        fn alias(&self) -> &str {
            "waiting-approval-fixture"
        }
    }

    #[async_trait::async_trait]
    impl Channel for WaitingChannel {
        fn name(&self) -> &str {
            "waiting-approval-fixture"
        }
        async fn send(&self, _: &zeroclaw_api::channel::SendMessage) -> anyhow::Result<()> {
            Ok(())
        }
        async fn listen(&self, _: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
            Ok(())
        }
        async fn request_approval_attributed(
            &self,
            _: &str,
            _: &zeroclaw_api::channel::ChannelApprovalRequest,
        ) -> anyhow::Result<Option<AttributedApprovalResponse>> {
            struct Dropped<'a>(&'a AtomicBool);
            impl Drop for Dropped<'_> {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let _guard = Dropped(&self.dropped);
            let rx = self.response.lock().take().expect("one prompt per fixture");
            self.ready.notify_one();
            Ok(Some(AttributedApprovalResponse::operator(rx.await??)))
        }
    }

    fn context<'a>(
        manager: &'a ApprovalManager,
        channel: &'a WaitingChannel,
        observer: &'a NoopObserver,
        pacing: &'a PacingConfig,
        cancellation: &'a CancellationToken,
    ) -> TurnCtx<'a> {
        TurnCtx {
            observer,
            provider_name: "test",
            model: "test",
            temperature: None,
            approval: Some(manager),
            channel_name: "approval-fixture",
            channel_reply_target: Some("fixture"),
            cancellation_token: Some(cancellation),
            on_delta: None,
            event_tx: None,
            hooks: None,
            dedup_exempt_tools: &[],
            pacing,
            strict_tool_parsing: false,
            channel: Some(channel),
            draft_reasoning: StreamReasoningMode::Status,
            turn_id: "approval-fixture",
            agent_alias: None,
            parent_agent_alias: None,
        }
    }

    fn manager() -> ApprovalManager {
        ApprovalManager::for_non_interactive(&RiskProfileConfig {
            always_ask: vec!["shell".into()],
            ..RiskProfileConfig::default()
        })
    }

    #[tokio::test]
    async fn cancelled_channel_approval_drops_waiter_without_operator_decision() {
        let manager = manager();
        let (channel, answer) = WaitingChannel::new();
        let cancellation = CancellationToken::new();
        let observer = NoopObserver;
        let pacing = PacingConfig::default();
        let ctx = context(&manager, &channel, &observer, &pacing, &cancellation);
        let args = serde_json::json!({"command":"fixture"});
        let mut gate = Box::pin(gate_tool_approval(&ctx, "shell", &args, 0));
        tokio::select! {
            biased;
            _ = &mut gate => panic!("approval completed before cancellation"),
            () = channel.ready.notified() => {}
        }
        cancellation.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), gate)
            .await
            .unwrap()
            .err()
            .expect("cancelled");
        assert!(super::super::outcome::is_tool_loop_cancelled(&error));
        assert!(channel.dropped.load(Ordering::SeqCst));
        assert!(
            answer
                .send(Ok(ChannelApprovalResponse::AlwaysApprove))
                .is_err()
        );
        assert!(manager.audit_log().is_empty());
        assert!(manager.session_allowlist().is_empty());
    }

    #[tokio::test]
    async fn channel_returned_estop_cause_is_preserved_without_operator_denial() {
        let manager = manager();
        let (channel, answer) = WaitingChannel::new();
        let cancellation = CancellationToken::new();
        let observer = NoopObserver;
        let pacing = PacingConfig::default();
        let ctx = context(&manager, &channel, &observer, &pacing, &cancellation);
        let args = serde_json::json!({});
        let mut gate = Box::pin(gate_tool_approval(&ctx, "shell", &args, 0));
        tokio::select! {
            biased;
            _ = &mut gate => panic!("approval completed before response"),
            () = channel.ready.notified() => {}
        }
        answer.send(Err(EstopInterrupted.into())).unwrap();
        let error = gate.await.err().expect("terminal interruption");
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert!(manager.audit_log().is_empty());
        assert!(manager.session_allowlist().is_empty());
    }

    #[tokio::test]
    async fn tool_freeze_interrupts_pending_channel_approval_without_decision() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = zeroclaw_config::schema::Config::default();
        config.security.estop.enabled = true;
        config.security.estop.state_file =
            tmp.path().join("estop.json").to_string_lossy().into_owned();
        let runtime = EstopRuntime::from_config(&config);
        let mut stop = EstopManager::load(&config.security.estop, tmp.path()).unwrap();
        let manager = manager();
        let (channel, answer) = WaitingChannel::new();
        let cancellation = CancellationToken::new();
        let observer = NoopObserver;
        let pacing = PacingConfig::default();
        let ctx = context(&manager, &channel, &observer, &pacing, &cancellation);
        let args = serde_json::json!({});
        let mut gate = Box::pin(estop_runtime::scope(
            Some(runtime),
            gate_tool_approval(&ctx, "shell", &args, 0),
        ));
        tokio::select! {
            biased;
            _ = &mut gate => panic!("approval completed before freeze"),
            () = channel.ready.notified() => {}
        }
        stop.engage(EstopLevel::ToolFreeze(vec!["shell".into()]))
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), gate)
            .await
            .unwrap()
            .err()
            .expect("frozen");
        assert!(estop_runtime::is_estop_interrupted(&error));
        assert!(channel.dropped.load(Ordering::SeqCst));
        assert!(answer.send(Ok(ChannelApprovalResponse::Approve)).is_err());
        assert!(manager.audit_log().is_empty());
        assert!(manager.session_allowlist().is_empty());
    }
}
