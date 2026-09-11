//! Lightweight LLM task tool for structured JSON-only sub-calls.

use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use zeroclaw_api::model_provider::ModelProvider;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;
use zeroclaw_providers::ProviderDispatch;

/// Tool that runs a single prompt through an LLM and optionally validates
/// the response against a JSON Schema. No tools are provided to the LLM —
/// this is a pure text-in, text-out (or JSON-out) call.
pub struct LlmTaskTool {
    security: Arc<SecurityPolicy>,
    /// Agent configuration is the source of truth for provider alias, auth,
    /// endpoint, model, and temperature. Runtime reload reconstructs this tool.
    config: Arc<zeroclaw_config::schema::Config>,
    agent_alias: String,
}

impl LlmTaskTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        config: Arc<zeroclaw_config::schema::Config>,
        agent_alias: String,
    ) -> Self {
        Self {
            security,
            config,
            agent_alias,
        }
    }

    fn model_provider(&self) -> anyhow::Result<Box<dyn ModelProvider>> {
        let (family, alias, entry) = self
            .config
            .resolved_model_provider_for_agent(&self.agent_alias)
            .ok_or_else(|| anyhow::Error::msg("Agent model_provider is not configured"))?;
        let options =
            zeroclaw_providers::provider_runtime_options_for_alias(&self.config, family, alias);
        zeroclaw_providers::create_model_provider_for_alias(
            &self.config,
            family,
            alias,
            entry.api_key.as_deref(),
            &options,
        )
    }
}

#[async_trait]
impl Tool for LlmTaskTool {
    fn name(&self) -> &str {
        "llm_task"
    }

    fn description(&self) -> &str {
        "Run a prompt through an LLM with no tool access and return the response. \
         Optionally validates the output against a JSON Schema. Ideal for structured \
         data extraction, classification, summarization, and transformation tasks."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "The prompt to send to the LLM."
                },
                "schema": {
                    "type": "object",
                    "description": "Optional JSON Schema to validate the LLM response against. \
                                    When provided, the LLM is instructed to return valid JSON \
                                    matching this schema."
                },
                "model": {
                    "type": "string",
                    "description": "Optional model override (e.g. 'anthropic/claude-sonnet-4-6'). \
                                    Defaults to the configured default model."
                },
                "temperature": {
                    "type": "number",
                    "description": "Optional temperature override (0.0-2.0). \
                                    Defaults to the configured default temperature."
                }
            },
            "required": ["prompt"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let result: anyhow::Result<ToolResult> = async {
            // Security gate
            if let Err(error) = self
                .security
                .enforce_tool_operation(ToolOperation::Act, "llm_task")
            {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(error),
                });
            }

            // Extract required prompt
            let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
                Some(p) if !p.trim().is_empty() => p,
                _ => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Missing or empty required parameter: prompt".to_string()),
                    });
                }
            };

            let entry = match self.config.model_provider_for_agent(&self.agent_alias) {
                Some(entry) => entry,
                None => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some("Agent model_provider is not configured".to_string()),
                    });
                }
            };

            // Extract optional overrides from the calling agent's profile.
            let schema = args.get("schema").and_then(|v| v.as_object());
            let model = args
                .get("model")
                .and_then(|v| v.as_str())
                .or(entry.model.as_deref())
                .unwrap_or("openai/gpt-4o-mini");
            let temperature = args
                .get("temperature")
                .and_then(|v| v.as_f64())
                .or(entry.temperature);

            // Build the effective prompt, adding JSON schema instructions when needed
            let effective_prompt = if let Some(schema_obj) = schema {
                let schema_json =
                    serde_json::to_string_pretty(&serde_json::Value::Object(schema_obj.clone()))
                        .unwrap_or_else(|_| "{}".to_string());
                format!(
                    "{prompt}\n\n\
                 IMPORTANT: You MUST respond with valid JSON that conforms to this schema:\n\
                 ```json\n{schema_json}\n```\n\
                 Respond ONLY with the JSON object, no explanation or markdown."
                )
            } else {
                prompt.to_string()
            };

            // Keep alias context so the factory can resolve profile-specific auth.
            let model_provider = match self.model_provider() {
                Ok(p) => p,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!("Failed to create provider: {e}")),
                    });
                }
            };

            // Make the LLM call (no tools, no agent loop). `temperature` is
            // already Option<f64>; pass straight through. None omits the field
            // on the wire so the provider applies its own default.
            let response = match ProviderDispatch::from_ref(&*model_provider)
                .simple_chat(&effective_prompt, model, temperature)
                .await
            {
                Ok(text) => text,
                Err(e) => {
                    return Ok(ToolResult {
                        success: false,
                        output: ToolOutput::default(),
                        error: Some(format!("LLM call failed: {e}")),
                    });
                }
            };

            // If schema was provided, validate the response
            if let Some(schema_obj) = schema {
                let schema_value = serde_json::Value::Object(schema_obj.clone());
                match validate_json_response(&response, &schema_value) {
                    Ok(validated_json) => Ok(ToolResult {
                        success: true,
                        output: validated_json.into(),
                        error: None,
                    }),
                    Err(validation_error) => Ok(ToolResult {
                        success: false,
                        output: response.into(),
                        error: Some(format!("Schema validation failed: {validation_error}")),
                    }),
                }
            } else {
                Ok(ToolResult {
                    success: true,
                    output: response.into(),
                    error: None,
                })
            }
        }
        .await;
        Ok(crate::output_budget::exact_read_result(result?))
    }
}

fn validate_json_response(response: &str, schema: &serde_json::Value) -> Result<String, String> {
    // Strip markdown code fences if the LLM wrapped the response
    let trimmed = response.trim();
    let json_str = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };

    // Parse as JSON
    let parsed: serde_json::Value =
        serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {e}"))?;

    // Check required fields
    if let Some(required) = schema.get("required").and_then(|v| v.as_array()) {
        for req in required {
            if let Some(field_name) = req.as_str()
                && parsed.get(field_name).is_none()
            {
                return Err(format!("Missing required field: {field_name}"));
            }
        }
    }

    // Check property types
    if let Some(properties) = schema.get("properties").and_then(|v| v.as_object()) {
        for (prop_name, prop_schema) in properties {
            if let Some(value) = parsed.get(prop_name)
                && let Some(expected_type) = prop_schema.get("type").and_then(|t| t.as_str())
                && !type_matches(value, expected_type)
            {
                return Err(format!(
                    "Field '{prop_name}' has wrong type: expected {expected_type}, \
                             got {}",
                    json_type_name(value)
                ));
            }
        }
    }

    // Return the cleaned, re-serialized JSON
    serde_json::to_string(&parsed).map_err(|e| format!("JSON serialization error: {e}"))
}

/// Check whether a JSON value matches an expected JSON Schema type string.
fn type_matches(value: &serde_json::Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true, // Unknown type — accept
    }
}

/// Return a human-readable type name for a JSON value.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(oauth: bool) -> zeroclaw_config::schema::Config {
        use zeroclaw_config::schema::{
            AliasedAgentConfig, Config, ModelProviderConfig, OpenAIModelProviderConfig,
        };
        let mut config = Config::default();
        config.providers.models.openai.insert(
            "task_profile".into(),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    model: Some("test-model".into()),
                    requires_openai_auth: oauth,
                    temperature: Some(0.7),
                    ..Default::default()
                },
            },
        );
        config.agents.insert(
            "test_agent".into(),
            AliasedAgentConfig {
                model_provider: "openai.task_profile".into(),
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn provider_preserves_oauth_alias_without_api_key() {
        let tool = LlmTaskTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(test_config(true)),
            "test_agent".into(),
        );
        let provider = tool
            .model_provider()
            .expect("OAuth alias should construct without API key");
        assert!(
            provider.capabilities().native_tool_calling,
            "must select the configured OAuth provider, not the API-key provider"
        );
    }

    // ── Schema validation tests ──────────────────────────────────────

    #[test]
    fn validate_valid_json_against_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "age": { "type": "integer" }
            },
            "required": ["name", "age"]
        });

        let response = r#"{"name": "Alice", "age": 30}"#;
        let result = validate_json_response(response, &schema);
        assert!(result.is_ok());

        let parsed: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
        assert_eq!(parsed["name"], "Alice");
        assert_eq!(parsed["age"], 30);
    }

    #[test]
    fn validate_missing_required_field() {
        let schema = json!({
            "type": "object",
            "properties": {
                "title": { "type": "string" },
                "score": { "type": "number" }
            },
            "required": ["title", "score"]
        });

        let response = r#"{"title": "Test"}"#;
        let result = validate_json_response(response, &schema);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("Missing required field: score")
        );
    }

    #[test]
    fn validate_wrong_type() {
        let schema = json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" }
            },
            "required": ["count"]
        });

        let response = r#"{"count": "not_a_number"}"#;
        let result = validate_json_response(response, &schema);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("wrong type"));
    }

    #[test]
    fn validate_strips_markdown_code_fences() {
        let schema = json!({
            "type": "object",
            "properties": {
                "result": { "type": "string" }
            },
            "required": ["result"]
        });

        let response = "```json\n{\"result\": \"ok\"}\n```";
        let result = validate_json_response(response, &schema);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_invalid_json() {
        let schema = json!({ "type": "object" });
        let response = "this is not json at all";
        let result = validate_json_response(response, &schema);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid JSON"));
    }

    #[test]
    fn validate_optional_fields_accepted() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "bio": { "type": "string" }
            },
            "required": ["name"]
        });

        // bio is optional, so this should pass
        let response = r#"{"name": "Bob"}"#;
        let result = validate_json_response(response, &schema);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_all_type_checks() {
        assert!(type_matches(&json!("hello"), "string"));
        assert!(!type_matches(&json!(42), "string"));

        assert!(type_matches(&json!(2.72), "number"));
        assert!(type_matches(&json!(42), "number"));
        assert!(!type_matches(&json!("42"), "number"));

        assert!(type_matches(&json!(42), "integer"));
        assert!(!type_matches(&json!(2.72), "integer"));

        assert!(type_matches(&json!(true), "boolean"));
        assert!(!type_matches(&json!(1), "boolean"));

        assert!(type_matches(&json!([1, 2]), "array"));
        assert!(!type_matches(&json!({}), "array"));

        assert!(type_matches(&json!({}), "object"));
        assert!(!type_matches(&json!([]), "object"));

        assert!(type_matches(&json!(null), "null"));

        // Unknown types are accepted
        assert!(type_matches(&json!("anything"), "custom_type"));
    }

    // ── Tool trait tests ─────────────────────────────────────────────

    #[test]
    fn tool_metadata() {
        let tool = LlmTaskTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(test_config(false)),
            "test_agent".to_string(),
        );

        assert_eq!(tool.name(), "llm_task");
        assert!(tool.description().contains("LLM"));

        let schema = tool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["prompt"].is_object());
        assert!(schema["properties"]["schema"].is_object());
        assert!(schema["properties"]["model"].is_object());
        assert!(schema["properties"]["temperature"].is_object());

        let required = schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "prompt");
    }

    #[tokio::test]
    async fn execute_missing_prompt_returns_error() {
        let tool = LlmTaskTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(test_config(false)),
            "test_agent".to_string(),
        );

        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn execute_empty_prompt_returns_error() {
        let tool = LlmTaskTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(test_config(false)),
            "test_agent".to_string(),
        );

        let result = tool.execute(json!({"prompt": "  "})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn execute_with_invalid_provider_returns_error() {
        let tool = LlmTaskTool::new(
            Arc::new(SecurityPolicy::default()),
            Arc::new(zeroclaw_config::schema::Config::default()),
            "missing_agent".to_string(),
        );

        let result = tool
            .execute(json!({"prompt": "Hello world"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("model_provider"));
    }
}
