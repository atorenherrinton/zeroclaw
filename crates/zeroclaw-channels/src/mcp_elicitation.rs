//! Session-bound MCP forms rendered through the channel's existing question UI.
//!
//! The ingress ConversationRoute is the source of recipient/sender identity.
//! Channel question registries own pending decisions; this bridge owns no global
//! approvals, persistence grants, or credentials.

use std::{collections::HashSet, future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use zeroclaw_api::{channel::Channel, conversation::ConversationRoute};
use zeroclaw_runtime::i18n::{get_required_cli_string, get_required_cli_string_with_args};
use zeroclaw_tools::mcp_protocol::{
    McpElicitationHandler, McpElicitationRequest, McpElicitationResult,
    with_mcp_elicitation_handler,
};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

fn text(key: &str) -> String {
    get_required_cli_string(key)
}

/// Scope the bridge only around an authenticated channel turn. A missing
/// channel leaves MCP's form requests unavailable; it never implies consent.
pub(crate) fn scope_channel_elicitation<'a, F>(
    channel: Option<Arc<dyn Channel>>,
    route: ConversationRoute,
    cancellation: CancellationToken,
    future: F,
) -> Pin<Box<dyn Future<Output = F::Output> + Send + 'a>>
where
    F: Future + Send + 'a,
    F::Output: Send + 'a,
{
    // Erase the large channel-body future before it enters the worker's nested
    // journal/deadline scopes. Its Send proof stays at this boundary instead of
    // overflowing trait evaluation through every outer worker wrapper.
    let future: Pin<Box<dyn Future<Output = F::Output> + Send + 'a>> = Box::pin(future);
    Box::pin(async move {
        if let Some(channel) = channel {
            let cancellation = cancellation.child_token();
            let _turn_end = cancellation.clone().drop_guard();
            with_mcp_elicitation_handler(
                Arc::new(ChannelElicitationHandler {
                    channel,
                    route,
                    cancellation,
                    task_grants: tokio::sync::Mutex::new(HashSet::new()),
                }),
                future,
            )
            .await
        } else {
            future.await
        }
    })
}

struct ChannelElicitationHandler {
    channel: Arc<dyn Channel>,
    route: ConversationRoute,
    cancellation: CancellationToken,
    // This set creates a new fact only after an explicit app-for-task choice.
    // It belongs to this authenticated turn and is never written to disk.
    task_grants: tokio::sync::Mutex<HashSet<String>>,
}

struct ChoiceField {
    name: String,
    title: String,
    values: Vec<Value>,
    labels: Vec<String>,
}

/// Native channel buttons can represent empty consent forms and bounded scalar
/// choices faithfully. Reject arbitrary/free-form/nested forms rather than
/// asking for secrets or silently discarding schema constraints.
fn choice_fields(schema: &Value) -> Result<Vec<ChoiceField>> {
    let invalid = || text("channel-elicitation-unsupported-form");
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow::Error::msg(invalid()))?;
    ensure!(
        object.get("type").and_then(Value::as_str) == Some("object"),
        "{}",
        invalid()
    );
    ensure!(
        object.keys().all(|key| matches!(
            key.as_str(),
            "type"
                | "properties"
                | "required"
                | "additionalProperties"
                | "$schema"
                | "title"
                | "description"
        )),
        "{}",
        invalid()
    );
    let properties = object
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::Error::msg(invalid()))?;
    ensure!(properties.len() <= 4, "{}", invalid());
    let required = object
        .get("required")
        .map(|v| v.as_array().ok_or_else(|| anyhow::Error::msg(invalid())))
        .transpose()?;
    if let Some(required) = required {
        ensure!(
            required.iter().all(|name| name
                .as_str()
                .is_some_and(|name| properties.contains_key(name))),
            "{}",
            invalid()
        );
    }
    let mut fields = Vec::new();
    for (name, property) in properties {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && !zeroclaw_api::elicitation::SENSITIVE_PROPERTY_NAMES
                    .iter()
                    .any(|s| name.eq_ignore_ascii_case(s)),
            "{}",
            invalid()
        );
        let property = property
            .as_object()
            .ok_or_else(|| anyhow::Error::msg(invalid()))?;
        ensure!(
            property.keys().all(|key| matches!(
                key.as_str(),
                "type" | "title" | "description" | "enum" | "enumNames" | "oneOf" | "default"
            )),
            "{}",
            invalid()
        );
        let mut title = property
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_owned();
        if let Some(description) = property.get("description").and_then(Value::as_str) {
            title.push('\n');
            title.push_str(description);
        }
        ensure!(title.len() <= 512, "{}", invalid());
        let (values, labels) = match property.get("type").and_then(Value::as_str) {
            Some("boolean")
                if !property.contains_key("enum") && !property.contains_key("oneOf") =>
            {
                (
                    vec![Value::Bool(true), Value::Bool(false)],
                    vec![
                        text("channel-elicitation-yes"),
                        text("channel-elicitation-no"),
                    ],
                )
            }
            Some("string") => {
                ensure!(
                    !(property.contains_key("enum") && property.contains_key("oneOf")),
                    "{}",
                    invalid()
                );
                if let Some(values) = property.get("enum").and_then(Value::as_array) {
                    ensure!(values.iter().all(Value::is_string), "{}", invalid());
                    let labels = if let Some(labels) = property.get("enumNames") {
                        let labels = labels
                            .as_array()
                            .ok_or_else(|| anyhow::Error::msg(invalid()))?;
                        ensure!(
                            labels.len() == values.len() && labels.iter().all(Value::is_string),
                            "{}",
                            invalid()
                        );
                        labels
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    } else {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    };
                    (values.clone(), labels)
                } else if let Some(options) = property.get("oneOf").and_then(Value::as_array) {
                    let mut values = Vec::new();
                    let mut labels = Vec::new();
                    for option in options {
                        let option = option
                            .as_object()
                            .ok_or_else(|| anyhow::Error::msg(invalid()))?;
                        ensure!(
                            option
                                .keys()
                                .all(|key| matches!(key.as_str(), "const" | "title")),
                            "{}",
                            invalid()
                        );
                        let value = option
                            .get("const")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::Error::msg(invalid()))?;
                        values.push(Value::String(value.to_owned()));
                        labels.push(
                            option
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or(value)
                                .to_owned(),
                        );
                    }
                    (values, labels)
                } else {
                    anyhow::bail!(invalid());
                }
            }
            _ => anyhow::bail!(invalid()),
        };
        ensure!(
            !values.is_empty()
                && values.len() <= 8
                && labels
                    .iter()
                    .all(|label| !label.is_empty() && label.len() <= 100),
            "{}",
            invalid()
        );
        ensure!(
            values
                .iter()
                .all(|value| value.as_str().is_none_or(|s| s.len() <= 1024)),
            "{}",
            invalid()
        );
        ensure!(
            values
                .iter()
                .enumerate()
                .all(|(index, value)| !values[..index].contains(value)),
            "{}",
            invalid()
        );
        // Index-prefixed labels preserve identity even if a server repeats names.
        let labels = labels
            .into_iter()
            .enumerate()
            .map(|(index, label)| format!("{}. {label}", index + 1))
            .collect();
        fields.push(ChoiceField {
            name: name.clone(),
            title,
            values,
            labels,
        });
    }
    Ok(fields)
}

fn prompt(request: &McpElicitationRequest) -> Result<String> {
    let mut parts = vec![
        get_required_cli_string_with_args(
            "channel-elicitation-source",
            &[
                ("server", &request.server_name),
                ("tool", &request.tool_name),
            ],
        ),
        request.message.clone(),
    ];
    for key in ["title", "description"] {
        if let Some(value) = request.requested_schema.get(key).and_then(Value::as_str) {
            parts.push(value.to_owned());
        }
    }
    if let Some(meta) = request.meta.as_ref().and_then(Value::as_object) {
        ensure!(
            meta.keys().all(|key| matches!(
                key.as_str(),
                "codex_approval_kind"
                    | "codex_request_type"
                    | "connector_id"
                    | "connector_name"
                    | "persist"
                    | "riskLevel"
                    | "subtitle"
                    | "tool_call_id"
                    | "tool_name"
                    | "tool_params"
                    | "tool_params_display"
            )),
            "{}",
            text("channel-elicitation-invalid-metadata")
        );
        for key in [
            "subtitle",
            "riskLevel",
            "tool_name",
            "connector_id",
            "connector_name",
            "codex_approval_kind",
            "codex_request_type",
            "tool_call_id",
        ] {
            ensure!(
                meta.get(key).is_none_or(Value::is_string),
                "{}",
                text("channel-elicitation-invalid-metadata")
            );
        }
        for (key, expected) in [
            ("codex_approval_kind", "mcp_tool_call"),
            ("codex_request_type", "approval_request"),
        ] {
            ensure!(
                meta.get(key).is_none_or(|value| value == expected),
                "{}",
                text("channel-elicitation-invalid-metadata")
            );
        }
        if let Some(persistence) = meta.get("persist") {
            ensure!(
                persistence
                    .as_array()
                    .is_some_and(|values| !values.is_empty()
                        && values
                            .iter()
                            .all(|value| matches!(value.as_str(), Some("session" | "always")))),
                "{}",
                text("channel-elicitation-invalid-metadata")
            );
        }
        for (key, label) in [
            ("subtitle", "channel-elicitation-warning"),
            ("riskLevel", "channel-elicitation-risk"),
            ("tool_name", "channel-elicitation-operation"),
        ] {
            if let Some(value) = meta.get(key).and_then(Value::as_str) {
                parts.push(get_required_cli_string_with_args(
                    label,
                    &[("value", value)],
                ));
            }
        }
        if let Some(parameters) = meta.get("tool_params") {
            ensure!(
                parameters.is_object(),
                "{}",
                text("channel-elicitation-invalid-metadata")
            );
            parts.push(get_required_cli_string_with_args(
                "channel-elicitation-target",
                &[("value", &serde_json::to_string(parameters)?)],
            ));
        }
        if let Some(parameters) = meta.get("tool_params_display") {
            ensure!(
                parameters
                    .as_array()
                    .is_some_and(|parameters| parameters.iter().all(|parameter| {
                        parameter.as_object().is_some_and(|parameter| {
                            parameter.keys().all(|key| {
                                matches!(key.as_str(), "name" | "display_name" | "value")
                            }) && parameter.get("name").is_none_or(Value::is_string)
                                && parameter.get("display_name").is_some_and(Value::is_string)
                                && parameter.get("value").is_some_and(Value::is_string)
                        })
                    })),
                "{}",
                text("channel-elicitation-invalid-metadata")
            );
        }
        if let Some(parameters) = meta.get("tool_params_display").and_then(Value::as_array) {
            for parameter in parameters {
                if let (Some(name), Some(value)) = (
                    parameter.get("display_name").and_then(Value::as_str),
                    parameter.get("value").and_then(Value::as_str),
                ) {
                    parts.push(format!("{name}: {value}"));
                }
            }
        }
    }
    let prompt = parts.join("\n\n");
    // Do not truncate approval context: refuse a form that cannot be presented.
    ensure!(
        prompt.len() <= 3000,
        "{}",
        text("channel-elicitation-prompt-too-large")
    );
    Ok(prompt)
}

/// Only the SDK's app-permission shape can offer a task grant. Include all
/// authorization context in its identity, excluding only the individual call
/// id and API operation (the explicit choice grants use of this app).
fn app_grant_key(request: &McpElicitationRequest) -> Result<Option<String>> {
    let Some(meta) = request.meta.as_ref().and_then(Value::as_object) else {
        return Ok(None);
    };
    if meta.get("connector_id").and_then(Value::as_str) != Some("computer-use") {
        return Ok(None);
    }
    let invalid = || text("channel-elicitation-invalid-metadata");
    ensure!(
        meta.keys().all(|key| matches!(
            key.as_str(),
            "codex_approval_kind"
                | "codex_request_type"
                | "connector_id"
                | "connector_name"
                | "persist"
                | "riskLevel"
                | "subtitle"
                | "tool_call_id"
                | "tool_name"
                | "tool_params"
                | "tool_params_display"
        )),
        "{}",
        invalid()
    );
    ensure!(
        meta.get("codex_approval_kind").and_then(Value::as_str) == Some("mcp_tool_call"),
        "{}",
        invalid()
    );
    ensure!(
        meta.get("codex_request_type")
            .is_none_or(|kind| kind == "approval_request"),
        "{}",
        invalid()
    );
    ensure!(
        meta.get("riskLevel")
            .and_then(Value::as_str)
            .is_some_and(|risk| !risk.is_empty()),
        "{}",
        invalid()
    );
    let persistence = meta
        .get("persist")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::Error::msg(invalid()))?;
    ensure!(
        !persistence.is_empty()
            && persistence
                .iter()
                .all(|value| matches!(value.as_str(), Some("session" | "always"))),
        "{}",
        invalid()
    );
    let params = meta
        .get("tool_params")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::Error::msg(invalid()))?;
    // Audio/screen and future permission classes receive individual decisions.
    if params.is_empty() {
        return Ok(None);
    }
    ensure!(
        params.len() == 1
            && params
                .get("app")
                .and_then(Value::as_str)
                .is_some_and(|app| !app.is_empty()),
        "{}",
        invalid()
    );
    if !persistence.iter().any(|value| value == "session") {
        return Ok(None);
    }
    let mut metadata = meta.clone();
    metadata.remove("tool_call_id");
    metadata.remove("tool_name");
    Ok(Some(serde_json::to_string(&serde_json::json!({
        "server": request.server_name,
        "instance": request.server_instance_id,
        "epoch": request.connection_epoch,
        "message": request.message,
        "schema": request.requested_schema,
        "metadata": metadata,
    }))?))
}

impl ChannelElicitationHandler {
    async fn choose(&self, question: &str, choices: &[String]) -> Result<Option<String>> {
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Ok(None),
            result = tokio::time::timeout(RESPONSE_TIMEOUT, self.channel.request_choice(question, choices, RESPONSE_TIMEOUT)) => {
                match result {
                    Ok(result) => result,
                    Err(_) => Ok(None),
                }
            }
        }
    }

    async fn form(&self, request: McpElicitationRequest) -> Result<McpElicitationResult> {
        let fields = choice_fields(&request.requested_schema)?;
        let heading = prompt(&request)?;
        let grant_key = if fields.is_empty() {
            app_grant_key(&request)?
        } else {
            None
        };
        if self.cancellation.is_cancelled() {
            return Ok(McpElicitationResult::Cancel);
        }
        if let Some(key) = &grant_key
            && self.task_grants.lock().await.contains(key)
        {
            if self.cancellation.is_cancelled() {
                return Ok(McpElicitationResult::Cancel);
            }
            return Ok(McpElicitationResult::Accept(Value::Object(Map::new())));
        }
        let decline = text("channel-elicitation-decline");
        let cancel = text("channel-elicitation-cancel");
        let mut content = Map::new();
        let mut summaries = Vec::new();
        for field in fields {
            let mut choices = field.labels.clone();
            choices.extend([decline.clone(), cancel.clone()]);
            let answer = self
                .choose(&format!("{heading}\n\n{}", field.title), &choices)
                .await?;
            match answer {
                Some(answer) if answer == decline => return Ok(McpElicitationResult::Decline),
                Some(answer) => {
                    let Some(index) = field.labels.iter().position(|label| label == &answer) else {
                        return Ok(McpElicitationResult::Cancel);
                    };
                    summaries.push(format!("{}: {}", field.title, field.labels[index]));
                    content.insert(field.name, field.values[index].clone());
                }
                None => return Ok(McpElicitationResult::Cancel),
            }
        }
        let accept = text("channel-elicitation-accept");
        let task_accept = text("channel-elicitation-accept-task");
        let mut choices = vec![accept.clone()];
        if grant_key.is_some() {
            choices.push(task_accept.clone());
        }
        choices.extend([decline.clone(), cancel]);
        let heading = if summaries.is_empty() {
            heading
        } else {
            format!("{heading}\n\n{}", summaries.join("\n"))
        };
        ensure!(
            heading.len() <= 3800,
            "{}",
            text("channel-elicitation-prompt-too-large")
        );
        let answer = self.choose(&heading, &choices).await?;
        if self.cancellation.is_cancelled() {
            return Ok(McpElicitationResult::Cancel);
        }
        match answer {
            Some(answer) if answer == accept => {
                Ok(McpElicitationResult::Accept(Value::Object(content)))
            }
            Some(answer) if answer == task_accept && grant_key.is_some() => {
                if self.cancellation.is_cancelled() {
                    return Ok(McpElicitationResult::Cancel);
                }
                if let Some(key) = grant_key {
                    let mut grants = self.task_grants.lock().await;
                    if self.cancellation.is_cancelled() {
                        return Ok(McpElicitationResult::Cancel);
                    }
                    grants.insert(key);
                }
                Ok(McpElicitationResult::Accept(Value::Object(content)))
            }
            Some(answer) if answer == decline => Ok(McpElicitationResult::Decline),
            _ => Ok(McpElicitationResult::Cancel),
        }
    }
}

#[async_trait]
impl McpElicitationHandler for ChannelElicitationHandler {
    async fn elicit(&self, request: McpElicitationRequest) -> Result<McpElicitationResult> {
        // Spawned work may not inherit task-locals. Restore only the route
        // captured from authenticated ingress, never server-provided metadata.
        zeroclaw_api::conversation::ACTIVE_CONVERSATION
            .scope(Some(self.route.clone()), self.form(request))
            .await
    }
}

/// A real terminal approval surface for the single-message CLI path. Callers
/// must not install it around a REPL or another owner of terminal input.
#[cfg(unix)]
pub struct ConsoleElicitationHandler {
    inner: ChannelElicitationHandler,
}

#[cfg(unix)]
impl ConsoleElicitationHandler {
    pub fn new(cancellation: CancellationToken) -> Result<Self> {
        use std::io::IsTerminal;
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "{}",
            text("channel-elicitation-terminal-required")
        );
        Ok(Self {
            inner: ChannelElicitationHandler {
                channel: Arc::new(ConsoleChoices {
                    active: tokio::sync::Mutex::new(()),
                }),
                route: ConversationRoute {
                    channel: "cli".into(),
                    recipient: "user".into(),
                    sender: "user".into(),
                    thread: None,
                    reply_to: String::new(),
                },
                cancellation,
                task_grants: tokio::sync::Mutex::new(HashSet::new()),
            },
        })
    }
}

#[cfg(unix)]
#[async_trait]
impl McpElicitationHandler for ConsoleElicitationHandler {
    async fn elicit(&self, request: McpElicitationRequest) -> Result<McpElicitationResult> {
        self.inner.elicit(request).await
    }
}

#[cfg(unix)]
struct ConsoleChoices {
    active: tokio::sync::Mutex<()>,
}

#[cfg(unix)]
impl zeroclaw_api::attribution::Attributable for ConsoleChoices {
    fn role(&self) -> zeroclaw_api::attribution::Role {
        zeroclaw_api::attribution::Role::Channel(zeroclaw_api::attribution::ChannelKind::Cli)
    }
    fn alias(&self) -> &str {
        "mcp-elicitation"
    }
}

#[cfg(unix)]
#[async_trait]
impl Channel for ConsoleChoices {
    fn name(&self) -> &str {
        "cli"
    }
    async fn start_typing(&self, _recipient: &str) -> Result<()> {
        Ok(())
    }
    async fn stop_typing(&self, _recipient: &str) -> Result<()> {
        Ok(())
    }
    async fn send(&self, _message: &zeroclaw_api::channel::SendMessage) -> Result<()> {
        anyhow::bail!(text("channel-elicitation-terminal-choice-only"))
    }
    async fn listen(&self, _tx: zeroclaw_api::inbound::Sender) -> Result<()> {
        anyhow::bail!(text("channel-elicitation-terminal-choice-only"))
    }
    fn supports_free_form_ask(&self) -> bool {
        false
    }

    async fn request_choice(
        &self,
        question: &str,
        choices: &[String],
        timeout: Duration,
    ) -> Result<Option<String>> {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let _active = self.active.lock().await;
        // Open a separate nonblocking descriptor. No detached blocking stdin
        // worker survives timeout or cancellation and consumes a later answer.
        let terminal = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
            .open("/dev/tty")?;
        // This surface is installed only for single-message commands, with no
        // competing stdin reader. Discard typeahead before showing a new prompt
        // so a late answer to a cancelled request cannot approve this request.
        if unsafe { libc::tcflush(terminal.as_raw_fd(), libc::TCIFLUSH) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let labels = choices
            .iter()
            .enumerate()
            .map(|(index, label)| format!("{}. {label}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let output = terminal_text(&format!(
            "\n{question}\n\n{labels}\n{} ",
            text("channel-elicitation-terminal-prompt")
        ));
        let operation = async {
            let mut terminal = &terminal;
            let mut pending = output.as_bytes();
            while !pending.is_empty() {
                match terminal.write(pending) {
                    Ok(0) => anyhow::bail!("terminal output closed"),
                    Ok(written) => pending = &pending[written..],
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            let mut answer = Vec::new();
            loop {
                let mut buffer = [0; 256];
                // macOS kqueue rejects some /dev/tty descriptors. Nonblocking
                // polling stays cancellation-safe without an orphan reader.
                let read = match terminal.read(&mut buffer) {
                    Ok(read) => read,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error.into()),
                };
                if read == 0 {
                    return Ok(None);
                }
                answer.extend_from_slice(&buffer[..read]);
                if answer.len() > 1024 {
                    return Ok(None);
                }
                if answer.contains(&b'\n') {
                    let answer = String::from_utf8(answer)?;
                    return Ok(answer
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| index.checked_sub(1))
                        .and_then(|index| choices.get(index))
                        .cloned());
                }
            }
        };
        match tokio::time::timeout(timeout, operation).await {
            Ok(result) => result,
            Err(_) => Ok(None),
        }
    }
}

#[cfg(unix)]
fn terminal_text(value: &str) -> String {
    // MCP text is untrusted. Render terminal escapes and bidi overrides
    // visibly instead of allowing them to hide or rewrite an approval prompt.
    value
        .chars()
        .map(|c| {
            if (c.is_control() && c != '\n' && c != '\t')
                || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                format!("\\u{{{:x}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Questions {
        answers: tokio::sync::Mutex<VecDeque<String>>,
        questions: tokio::sync::Mutex<Vec<String>>,
        routes: tokio::sync::Mutex<Vec<ConversationRoute>>,
        stall: bool,
    }
    impl zeroclaw_api::attribution::Attributable for Questions {
        fn role(&self) -> zeroclaw_api::attribution::Role {
            zeroclaw_api::attribution::Role::Channel(zeroclaw_api::attribution::ChannelKind::Cli)
        }
        fn alias(&self) -> &str {
            "fixture"
        }
    }
    #[async_trait]
    impl Channel for Questions {
        fn name(&self) -> &str {
            "fixture"
        }
        async fn start_typing(&self, _recipient: &str) -> Result<()> {
            Ok(())
        }
        async fn stop_typing(&self, _recipient: &str) -> Result<()> {
            Ok(())
        }
        async fn send(&self, _message: &zeroclaw_api::channel::SendMessage) -> Result<()> {
            Ok(())
        }
        async fn listen(&self, _tx: zeroclaw_api::inbound::Sender) -> Result<()> {
            Ok(())
        }
        async fn request_choice(
            &self,
            question: &str,
            _choices: &[String],
            _timeout: Duration,
        ) -> Result<Option<String>> {
            self.questions.lock().await.push(question.to_owned());
            if let Some(route) = zeroclaw_api::conversation::current() {
                self.routes.lock().await.push(route);
            }
            if self.stall {
                std::future::pending::<()>().await;
            }
            Ok(self.answers.lock().await.pop_front())
        }
    }
    fn route() -> ConversationRoute {
        ConversationRoute {
            channel: "telegram.fixture".into(),
            recipient: "room:thread".into(),
            sender: "owner".into(),
            thread: Some("thread".into()),
            reply_to: "message".into(),
        }
    }
    fn request() -> McpElicitationRequest {
        McpElicitationRequest {
            server_name: "computer-fixture".into(),
            server_instance_id: "instance".into(),
            connection_epoch: 1,
            originating_request_id: json!(9),
            request_id: json!("permission"),
            tool_name: "js".into(),
            message: "Allow Computer Use to use Fixture App?".into(),
            requested_schema: json!({"type":"object","properties":{}}),
            meta: Some(
                json!({"codex_approval_kind":"mcp_tool_call","connector_id":"computer-use","connector_name":"Computer Use","persist":["session","always"],"riskLevel":"high","subtitle":"Can read and change this app","tool_name":"get_app_state","tool_params":{"app":"org.example.fixture"},"tool_params_display":[{"name":"app","display_name":"App","value":"Fixture App"}]}),
            ),
        }
    }
    fn handler(questions: Arc<Questions>) -> ChannelElicitationHandler {
        ChannelElicitationHandler {
            channel: questions,
            route: route(),
            cancellation: CancellationToken::new(),
            task_grants: tokio::sync::Mutex::new(HashSet::new()),
        }
    }
    #[tokio::test]
    async fn empty_form_requires_real_choice_and_uses_original_route() {
        for (choice, expected) in [
            ("channel-elicitation-accept", "accept"),
            ("channel-elicitation-decline", "decline"),
            ("channel-elicitation-cancel", "cancel"),
        ] {
            let questions = Arc::new(Questions::default());
            questions.answers.lock().await.push_back(text(choice));
            let handler = handler(questions.clone());
            let result = handler.elicit(request()).await.unwrap();
            assert!(matches!(
                (result, expected),
                (McpElicitationResult::Accept(_), "accept")
                    | (McpElicitationResult::Decline, "decline")
                    | (McpElicitationResult::Cancel, "cancel")
            ));
            assert_eq!(questions.routes.lock().await.as_slice(), &[route()]);
            let prompts = questions.questions.lock().await;
            assert!(prompts[0].contains("org.example.fixture"));
            assert!(prompts[0].contains("high"));
            assert!(prompts[0].contains("Can read and change this app"));
        }
        assert!(zeroclaw_api::conversation::current().is_none());
    }
    #[tokio::test]
    async fn app_task_grant_requires_explicit_choice_and_exact_context() {
        let questions = Arc::new(Questions::default());
        questions
            .answers
            .lock()
            .await
            .push_back(text("channel-elicitation-accept-task"));
        let handler = handler(questions.clone());
        assert!(matches!(
            handler.elicit(request()).await.unwrap(),
            McpElicitationResult::Accept(_)
        ));
        let mut same_app = request();
        same_app.meta.as_mut().unwrap()["tool_name"] = json!("click");
        same_app.meta.as_mut().unwrap()["tool_call_id"] = json!("another-call");
        assert!(matches!(
            handler.elicit(same_app).await.unwrap(),
            McpElicitationResult::Accept(_)
        ));
        assert_eq!(questions.questions.lock().await.len(), 1);
        for field in ["app", "risk", "warning", "epoch", "instance"] {
            let mut changed = request();
            match field {
                "app" => {
                    changed.meta.as_mut().unwrap()["tool_params"]["app"] =
                        json!("org.example.other")
                }
                "risk" => changed.meta.as_mut().unwrap()["riskLevel"] = json!("other"),
                "warning" => changed.meta.as_mut().unwrap()["subtitle"] = json!("Different access"),
                "epoch" => changed.connection_epoch = 2,
                _ => changed.server_instance_id = "replacement-instance".into(),
            }
            assert!(matches!(
                handler.elicit(changed).await.unwrap(),
                McpElicitationResult::Cancel
            ));
        }
        handler.cancellation.cancel();
        assert!(matches!(
            handler.elicit(request()).await.unwrap(),
            McpElicitationResult::Cancel
        ));
        assert_eq!(questions.questions.lock().await.len(), 6);
    }
    #[tokio::test]
    async fn approve_once_does_not_create_task_grant() {
        let questions = Arc::new(Questions::default());
        questions
            .answers
            .lock()
            .await
            .push_back(text("channel-elicitation-accept"));
        let handler = handler(questions.clone());
        assert!(matches!(
            handler.elicit(request()).await.unwrap(),
            McpElicitationResult::Accept(_)
        ));
        assert!(matches!(
            handler.elicit(request()).await.unwrap(),
            McpElicitationResult::Cancel
        ));
        assert_eq!(questions.questions.lock().await.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_or_cancelled_question_never_accepts() {
        for cancel in [false, true] {
            let questions = Arc::new(Questions {
                stall: true,
                ..Default::default()
            });
            let handler = Arc::new(handler(questions.clone()));
            let waiting = handler.clone();
            let task = zeroclaw_spawn::spawn!(async move { waiting.elicit(request()).await });
            tokio::task::yield_now().await;
            assert_eq!(questions.questions.lock().await.len(), 1);
            if cancel {
                handler.cancellation.cancel();
            } else {
                tokio::time::advance(RESPONSE_TIMEOUT + Duration::from_secs(1)).await;
            }
            assert!(matches!(
                task.await.unwrap().unwrap(),
                McpElicitationResult::Cancel
            ));
            assert!(handler.task_grants.lock().await.is_empty());
        }
    }
    #[tokio::test]
    async fn form_returns_selected_wire_value_and_shows_final_answers() {
        let questions = Arc::new(Questions::default());
        questions
            .answers
            .lock()
            .await
            .extend(["2. Second".to_owned(), text("channel-elicitation-accept")]);
        let handler = handler(questions.clone());
        let mut request = request();
        request.meta = None;
        request.requested_schema = json!({"type":"object","properties":{"choice":{"type":"string","title":"Selection","enum":["first","second"],"enumNames":["First","Second"]}},"required":["choice"]});
        let McpElicitationResult::Accept(content) = handler.elicit(request).await.unwrap() else {
            panic!("expected accepted form");
        };
        assert_eq!(content, json!({"choice":"second"}));
        assert!(questions.questions.lock().await[1].contains("Selection: 2. Second"));
    }
    #[tokio::test]
    async fn malformed_metadata_is_rejected_before_any_question() {
        let questions = Arc::new(Questions::default());
        let handler = handler(questions.clone());
        for field in [
            "risk",
            "target",
            "unknown",
            "display",
            "display-extra",
            "persist",
            "kind",
        ] {
            let mut request = request();
            match field {
                "risk" => request.meta.as_mut().unwrap()["riskLevel"] = json!({"hidden":"risk"}),
                "target" => {
                    request.meta.as_mut().unwrap()["tool_params"] =
                        json!({"app":"org.example.fixture","extra":"hidden scope"})
                }
                "unknown" => request.meta.as_mut().unwrap()["hidden_approval_scope"] = json!(true),
                "persist" => request.meta.as_mut().unwrap()["persist"] = json!("always"),
                "kind" => {
                    request.meta.as_mut().unwrap()["codex_approval_kind"] =
                        json!("different-approval")
                }
                "display-extra" => {
                    request.meta.as_mut().unwrap()["tool_params_display"][0]["hidden"] =
                        json!("extra scope")
                }
                _ => {
                    request.meta.as_mut().unwrap()["tool_params_display"] =
                        json!([{"display_name":"App","value":{"hidden":"label"}}])
                }
            }
            assert!(handler.elicit(request).await.is_err());
        }
        assert!(questions.questions.lock().await.is_empty());
    }
    #[test]
    fn rejects_sensitive_free_form_nested_and_unknown_constraints() {
        for property in [
            json!({"type":"string"}),
            json!({"type":"object"}),
            json!({"type":"string","enum":["a"],"pattern":"secret"}),
        ] {
            assert!(
                choice_fields(&json!({"type":"object","properties":{"answer":property}})).is_err()
            );
        }
        assert!(
            choice_fields(
                &json!({"type":"object","properties":{"password":{"type":"string","enum":["a"]}}})
            )
            .is_err()
        );
        assert!(
            choice_fields(&json!({"type":"object","properties":{},"required":["missing"]}))
                .is_err()
        );
        assert!(choice_fields(&json!({"type":"object","properties":{"answer":{"type":"string","oneOf":[{"const":"same","title":"First"},{"const":"same","title":"Second"}]}}})).is_err());
    }
    #[tokio::test]
    async fn scoped_handler_is_revoked_at_turn_end() {
        let questions = Arc::new(Questions::default());
        let channel: Arc<dyn Channel> = questions.clone();
        let inherited =
            scope_channel_elicitation(Some(channel), route(), CancellationToken::new(), async {
                zeroclaw_tools::mcp_protocol::current_mcp_elicitation_handler().unwrap()
            })
            .await;
        assert!(zeroclaw_tools::mcp_protocol::current_mcp_elicitation_handler().is_none());
        assert!(matches!(
            inherited.elicit(request()).await.unwrap(),
            McpElicitationResult::Cancel
        ));
        assert!(questions.questions.lock().await.is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn terminal_prompt_escapes_control_sequences() {
        assert_eq!(
            terminal_text("\u{1b}[2JAllow\u{202e}?\n"),
            "\\u{1b}[2JAllow\\u{202e}?\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "Requires a real TTY; enter 2 (Decline), then 3 (Cancel) after each prompt"]
    async fn console_tty_decline_and_cancel() {
        // A synthetic empty form exercises the real terminal path without
        // contacting an MCP server, opening an app, or granting app access.
        let handler = ConsoleElicitationHandler::new(CancellationToken::new()).unwrap();
        let mut synthetic = request();
        synthetic.server_name = "synthetic-console-test".into();
        synthetic.tool_name = "no-app-access".into();
        synthetic.meta = None;
        synthetic.message = "Synthetic console test only: choose Decline (2). This test does not open or authorize any app.".into();
        assert_eq!(
            handler.elicit(synthetic.clone()).await.unwrap(),
            McpElicitationResult::Decline
        );
        synthetic.message = "Synthetic console test only: choose Cancel (3). This test does not open or authorize any app.".into();
        assert_eq!(
            handler.elicit(synthetic).await.unwrap(),
            McpElicitationResult::Cancel
        );
    }
}
