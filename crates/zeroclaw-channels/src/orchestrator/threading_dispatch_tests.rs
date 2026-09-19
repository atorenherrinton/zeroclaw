mod threading_dispatch_tests {
    use super::*;
    use tokio::sync::{mpsc, oneshot};

    struct ProviderRequest {
        history: Vec<ChatMessage>,
        release: oneshot::Sender<()>,
    }

    struct RequestLifetime {
        label: String,
        completed: bool,
        cancelled: mpsc::UnboundedSender<String>,
    }

    impl Drop for RequestLifetime {
        fn drop(&mut self) {
            if !self.completed {
                let _ = self.cancelled.send(self.label.clone());
            }
        }
    }

    struct ControlledProvider {
        requests: mpsc::UnboundedSender<ProviderRequest>,
        cancelled: mpsc::UnboundedSender<String>,
    }

    impl zeroclaw_api::attribution::Attributable for ControlledProvider {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Provider(
                zeroclaw_api::attribution::ProviderKind::Model(
                    zeroclaw_api::attribution::ModelProviderKind::Custom,
                ),
            )
        }

        fn alias(&self) -> &str {
            "thread-dispatch-fixture"
        }
    }

    #[async_trait::async_trait]
    impl ModelProvider for ControlledProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            anyhow::bail!("history-aware provider path required")
        }

        async fn chat_with_history(
            &self,
            messages: &[ChatMessage],
            _model: &str,
            _temperature: Option<f64>,
        ) -> anyhow::Result<String> {
            let owner_message = messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .expect("request has owner message");
            let label = fixture_label(&owner_message.content).to_string();
            let mut lifetime = RequestLifetime {
                label: label.clone(),
                completed: false,
                cancelled: self.cancelled.clone(),
            };
            let (release, wait) = oneshot::channel();
            self.requests
                .send(ProviderRequest {
                    history: messages.to_vec(),
                    release,
                })
                .map_err(|_| anyhow::Error::msg("request observer closed"))?;
            wait.await?;
            lifetime.completed = true;
            Ok(format!("Completed: {label}"))
        }
    }

    struct TopicChannel(mpsc::UnboundedSender<SendMessage>);

    impl zeroclaw_api::attribution::Attributable for TopicChannel {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Channel(
                zeroclaw_api::attribution::ChannelKind::Webhook,
            )
        }

        fn alias(&self) -> &str {
            "thread-dispatch-fixture"
        }
    }

    #[async_trait::async_trait]
    impl Channel for TopicChannel {
        fn name(&self) -> &str {
            "telegram"
        }

        async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
            self.0
                .send(message.clone())
                .map_err(|_| anyhow::Error::msg("delivery observer closed"))
        }

        async fn listen(&self, _tx: zeroclaw_api::inbound::Sender) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn topic_message(id: &str, topic: &str, text: &str) -> ChannelMessage {
        ChannelMessage {
            id: format!("900000031:{id}"),
            sender: "fixture-owner".into(),
            reply_target: format!("900000031:{topic}"),
            content: text.into(),
            channel: "telegram".into(),
            thread_ts: Some(topic.into()),
            interruption_scope_id: Some(topic.into()),
            ..Default::default()
        }
    }

    async fn receive<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> T {
        tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("dispatcher made no progress")
            .expect("observer closed unexpectedly")
    }

    fn user_history(request: &ProviderRequest) -> Vec<&str> {
        request
            .history
            .iter()
            .filter(|message| message.role == "user")
            .map(|message| fixture_label(&message.content))
            .collect()
    }

    fn fixture_label(content: &str) -> &str {
        ["alpha-followup", "alpha-first", "beta-first"]
            .into_iter()
            .find(|label| content.ends_with(*label))
            .expect("request ends in a known fixture owner message")
    }

    #[test]
    fn telegram_topics_dispatch_concurrently_preserve_followups_and_isolate_stop() {
        run_channel_dispatch_test(|| async {
            let (requests, mut request_rx) = mpsc::unbounded_channel();
            let (cancelled, mut cancelled_rx) = mpsc::unbounded_channel();
            let (sent, mut sent_rx) = mpsc::unbounded_channel();
            let channel: Arc<dyn Channel> = Arc::new(TopicChannel(sent));
            let provider: Arc<dyn ModelProvider> = Arc::new(ControlledProvider {
                requests,
                cancelled,
            });
            let ctx = test_runtime_ctx_with_config_agent_and_provider_ref(
                channel,
                provider,
                Config::default(),
                zeroclaw_config::schema::AliasedAgentConfig::default(),
                "test-provider",
                None,
            );
            let (tx, rx) = zeroclaw_api::inbound::channel(8);
            let dispatch =
                run_message_dispatch_loop_supervised(rx, AgentRouter::single(ctx.clone()), 2, None);
            let dispatcher = zeroclaw_spawn::spawn!(dispatch);

            let first = topic_message("101", "11", "alpha-first");
            let second = topic_message("102", "22", "beta-first");
            tx.send(first.clone()).await.unwrap();
            let alpha = receive(&mut request_rx).await;
            assert_eq!(user_history(&alpha), ["alpha-first"]);

            // Alpha cannot complete until explicitly released. Reaching the
            // second provider proves actual concurrent dispatch, not timing.
            tx.send(second).await.unwrap();
            let beta = receive(&mut request_rx).await;
            assert_eq!(user_history(&beta), ["beta-first"]);
            let beta_system = beta.history.iter().find(|m| m.role == "system").unwrap();
            assert!(beta_system.content.contains("alpha-first"));
            assert!(beta_system.content.contains("[Live task coordination]"));

            // A same-topic follow-up must await alpha, then load its completed
            // answer. Beta remains blocked throughout this transition.
            tx.send(topic_message("103", "11", "alpha-followup"))
                .await
                .unwrap();
            alpha.release.send(()).unwrap();
            let followup = receive(&mut request_rx).await;
            assert_eq!(user_history(&followup), ["alpha-first", "alpha-followup"]);
            assert!(
                followup
                    .history
                    .iter()
                    .any(|m| { m.role == "assistant" && m.content == "Completed: alpha-first" })
            );
            assert!(
                followup
                    .history
                    .iter()
                    .any(|m| { m.role == "system" && m.content.contains("beta-first") })
            );

            tx.send(topic_message("104", "22", "/stop")).await.unwrap();
            assert_eq!(receive(&mut cancelled_rx).await, "beta-first");
            assert!(
                beta.release.send(()).is_err(),
                "stopped provider was dropped"
            );
            followup.release.send(()).unwrap();
            drop(tx);
            tokio::time::timeout(Duration::from_secs(30), dispatcher)
                .await
                .expect("dispatcher did not settle")
                .unwrap();
            assert!(cancelled_rx.try_recv().is_err(), "alpha was not cancelled");

            let mut deliveries = Vec::new();
            while let Ok(message) = sent_rx.try_recv() {
                deliveries.push(message);
            }
            assert!(deliveries.iter().any(|m| {
                m.recipient == "900000031:11" && m.content == "Completed: alpha-followup"
            }));
            assert!(
                !deliveries
                    .iter()
                    .any(|m| m.content == "Completed: beta-first")
            );
            let histories = ctx.conversation_histories.lock().unwrap();
            let saved = histories.peek(&conversation_history_key(&first)).unwrap();
            assert!(
                saved.iter().all(|m| {
                    !m.content.contains("[Live task coordination]")
                        && !m.content.contains("beta-first")
                }),
                "peer context must not be persisted into alpha history"
            );
        });
    }
}
