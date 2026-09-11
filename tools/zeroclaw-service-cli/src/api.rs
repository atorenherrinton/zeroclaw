use anyhow::{Result, bail};
use reqwest::{Client, Method, Url, header};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use zeroize::Zeroizing;

const MAX_RESPONSE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 24 * 1024;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Read {
    VercelProjects {
        team_id: Option<String>,
        cursor: Option<u64>,
    },
    VercelDeployments {
        project: String,
        team_id: Option<String>,
        cursor: Option<u64>,
    },
    VercelEnv {
        project: String,
        team_id: Option<String>,
    },
    ResendDomains {
        after: Option<String>,
    },
    ResendDomain {
        id: String,
    },
    ResendApiKeys {
        after: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopyResendKey {
    pub project: String,
    pub team_id: Option<String>,
    pub target: String,
    pub owner_requested: bool,
}

pub struct Plan {
    pub slot: &'static str,
    pub url: Url,
    pub kind: &'static str,
}

fn identifier(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 200
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("Invalid resource identifier; use only letters, numbers, hyphens, and underscores");
    }
    Ok(())
}

fn endpoint(slot: &'static str, path: &str, team: Option<&str>) -> Result<Url> {
    let origin = if slot == "vercel" {
        "https://api.vercel.com"
    } else {
        "https://api.resend.com"
    };
    let mut url = Url::parse(&format!("{origin}{path}"))?;
    if let Some(team) = team {
        identifier(team)?;
        url.query_pairs_mut().append_pair("teamId", team);
    }
    Ok(url)
}

pub fn plan(read: Read) -> Result<Plan> {
    let (slot, path, team, kind, cursor, after) = match read {
        Read::VercelProjects { team_id, cursor } => (
            "vercel",
            "/v10/projects".to_owned(),
            team_id,
            "projects",
            cursor,
            None,
        ),
        Read::VercelDeployments {
            project,
            team_id,
            cursor,
        } => {
            identifier(&project)?;
            let mut url = endpoint("vercel", "/v6/deployments", team_id.as_deref())?;
            url.query_pairs_mut()
                .append_pair("projectId", &project)
                .append_pair("limit", "20");
            if let Some(cursor) = cursor {
                url.query_pairs_mut()
                    .append_pair("until", &cursor.to_string());
            }
            return Ok(Plan {
                slot: "vercel",
                url,
                kind: "deployments",
            });
        }
        Read::VercelEnv { project, team_id } => {
            identifier(&project)?;
            (
                "vercel",
                format!("/v10/projects/{project}/env"),
                team_id,
                "env",
                None,
                None,
            )
        }
        Read::ResendDomains { after } => (
            "resend",
            "/domains".to_owned(),
            None,
            "domains",
            None,
            after,
        ),
        Read::ResendDomain { id } => {
            identifier(&id)?;
            (
                "resend",
                format!("/domains/{id}"),
                None,
                "domain",
                None,
                None,
            )
        }
        Read::ResendApiKeys { after } => (
            "resend",
            "/api-keys".to_owned(),
            None,
            "api_keys",
            None,
            after,
        ),
    };
    let mut url = endpoint(slot, &path, team.as_deref())?;
    if kind == "env" {
        url.query_pairs_mut().append_pair("decrypt", "false");
    } else if kind != "domain" {
        url.query_pairs_mut().append_pair("limit", "20");
    }
    if let Some(cursor) = cursor {
        url.query_pairs_mut()
            .append_pair("until", &cursor.to_string());
    }
    if let Some(after) = after {
        identifier(&after)?;
        url.query_pairs_mut().append_pair("after", &after);
    }
    Ok(Plan { slot, url, kind })
}

pub fn copy_plan(args: &CopyResendKey) -> Result<Plan> {
    if !args.owner_requested {
        bail!("An explicit owner request for this project and environment is required");
    }
    identifier(&args.project)?;
    if !matches!(args.target.as_str(), "production" | "preview") {
        bail!("Target must be production or preview");
    }
    let mut url = endpoint(
        "vercel",
        &format!("/v10/projects/{}/env", args.project),
        args.team_id.as_deref(),
    )?;
    url.query_pairs_mut().append_pair("upsert", "true");
    Ok(Plan {
        slot: "vercel",
        url,
        kind: "copy_resend_key",
    })
}

pub fn client() -> Result<Client> {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(25))
        .user_agent("ZeroClaw-Service-CLI/0.1.0")
        .build()
        .map_err(|_| anyhow::Error::msg("Could not initialize the HTTPS client"))
}

pub fn auth_header(token: &[u8]) -> Result<header::HeaderValue> {
    crate::credentials::validate_token(token)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(token.len() + 7));
    bytes.extend_from_slice(b"Bearer ");
    bytes.extend_from_slice(token);
    let mut value = header::HeaderValue::from_bytes(&bytes)
        .map_err(|_| anyhow::Error::msg("Invalid token encoding"))?;
    value.set_sensitive(true);
    Ok(value)
}

pub async fn execute(
    client: &Client,
    plan: &Plan,
    token: &[u8],
    body: Option<Vec<u8>>,
) -> Result<Value> {
    let write = body.is_some();
    let mut request = client
        .request(
            if write { Method::POST } else { Method::GET },
            plan.url.clone(),
        )
        .header(header::AUTHORIZATION, auth_header(token)?);
    if let Some(body) = body {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }
    // No request logging, redirects, endpoint overrides, or transport retries.
    let mut response = match request.send().await {
        Ok(response) => response,
        Err(_) => {
            return Ok(
                json!({"status":if write {"uncertain"} else {"transport_error"},
            "retry_write":false,"message":"No usable response. Reconcile writes before retrying; credentials and transport details are omitted."}),
            );
        }
    };
    let status = response.status();
    if !status.is_success() {
        // Error bodies can echo authentication or submitted secret values.
        return Ok(
            json!({"status":if write && status.is_server_error() {"uncertain"} else {"http_error"},
            "http_status":status.as_u16(),"retry_write":false,
            "message":"Service rejected the request or could not complete it. Raw error body omitted."}),
        );
    }
    let decoded: Result<Value> = async {
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::Error::msg("Response stream incomplete; body omitted"))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                bail!("Response exceeded 1 MiB; narrow the request");
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::Error::msg("Service returned invalid JSON; body omitted"))
    }
    .await;
    let raw = match decoded {
        Ok(raw) => raw,
        Err(_) if write => {
            return Ok(json!({"status":"uncertain",
            "http_status":status.as_u16(),"retry_write":false,
            "message":"The update response could not be read. Reconcile before retrying; body omitted."}));
        }
        Err(error) => return Err(error),
    };
    if write {
        let confirmed = raw["created"]["key"] == "RESEND_API_KEY"
            && raw["failed"]
                .as_array()
                .is_some_and(|items| items.is_empty());
        return Ok(
            json!({"status":if confirmed {"accepted"} else {"uncertain"},
            "http_status":status.as_u16(),"retry_write":false,"key":"RESEND_API_KEY",
            "value_returned":false,"message":if confirmed {
                "Vercel confirmed the environment update. Redeploy separately when authorized."
            } else { "The response did not confirm this update. Reconcile before retrying; body omitted." }}),
        );
    }
    project(plan.kind, &raw, token)
}

fn fields(value: &Value, names: &[&str]) -> Value {
    let mut result = serde_json::Map::new();
    for name in names {
        if let Some(value) = value.get(*name) {
            if value.is_string() || value.is_number() || value.is_boolean() || value.is_null() {
                result.insert((*name).into(), value.clone());
            } else if *name == "target"
                && let Some(values) = value.as_array()
            {
                result.insert(
                    (*name).into(),
                    Value::Array(
                        values
                            .iter()
                            .filter(|v| {
                                matches!(v.as_str(), Some("production" | "preview" | "development"))
                            })
                            .cloned()
                            .collect(),
                    ),
                );
            }
        }
    }
    Value::Object(result)
}

fn redact(value: &mut Value, token: &str) {
    match value {
        Value::String(text) => {
            *text = text.replace(token, "[REDACTED]");
            if text.len() > 512 {
                *text = text.chars().take(512).collect::<String>() + "…";
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| redact(item, token)),
        Value::Object(items) => items.values_mut().for_each(|item| redact(item, token)),
        _ => (),
    }
}

pub fn project(kind: &str, raw: &Value, token: &[u8]) -> Result<Value> {
    let allowed = match kind {
        "projects" => &["id", "name", "framework", "updatedAt"][..],
        "deployments" => &[
            "uid",
            "name",
            "url",
            "state",
            "readyState",
            "target",
            "created",
        ][..],
        "env" => &["id", "key", "type", "target", "updatedAt"][..],
        "domains" | "domain" => &["id", "name", "status", "region", "created_at"][..],
        "api_keys" => &["id", "name", "created_at", "last_used_at"][..],
        _ => bail!("Unknown response projection"),
    };
    let mut result = if kind == "domain" {
        fields(raw, allowed)
    } else {
        let list = if kind == "env" {
            raw.get("envs").or_else(|| raw.as_array().map(|_| raw))
        } else if kind == "projects" || kind == "deployments" {
            raw.get(kind)
        } else {
            raw.get("data")
        };
        let items = list
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::Error::msg("Unexpected service response shape; body omitted"))?;
        // Environment lists are not server-paginated. Return metadata for up to
        // 100 entries and state truncation; never include values or comments.
        let cap = if kind == "env" { 100 } else { 20 };
        json!({"items":items.iter().take(cap).map(|item| fields(item,allowed)).collect::<Vec<_>>(),
            "truncated":items.len()>cap,
            "next_cursor":raw.pointer("/pagination/next").and_then(Value::as_u64),
            "has_more":raw.get("has_more").and_then(Value::as_bool),
            "next_after":if raw.get("has_more")==Some(&Value::Bool(true)) {items.last().and_then(|v|v.get("id")).and_then(Value::as_str)} else {None}})
    };
    if kind == "domain"
        && let Some(records) = raw.get("records").and_then(Value::as_array)
    {
        result["records"] = Value::Array(
            records
                .iter()
                .take(30)
                .map(|r| fields(r, &["record", "name", "type", "ttl", "status", "priority"]))
                .collect(),
        );
        // DNS values are deliberately omitted: this tool diagnoses status,
        // not arbitrary TXT contents which can contain verification tokens.
    }
    let token =
        std::str::from_utf8(token).map_err(|_| anyhow::Error::msg("Invalid credential format"))?;
    redact(&mut result, token);
    if serde_json::to_vec(&result)?.len() > MAX_OUTPUT {
        bail!("Metadata exceeds output budget; narrow the request");
    }
    Ok(
        json!({"status":"ok","untrusted_service_data":true,"data":result,"secret_values_returned":false}),
    )
}

pub fn secret_body(target: &str, secret: &[u8]) -> Result<Vec<u8>> {
    let text =
        std::str::from_utf8(secret).map_err(|_| anyhow::Error::msg("Invalid secret format"))?;
    // Avoid storing a second plaintext String in a serde_json::Value tree.
    #[derive(serde::Serialize)]
    struct Body<'a> {
        key: &'static str,
        value: &'a str,
        r#type: &'static str,
        target: [&'a str; 1],
    }
    Ok(serde_json::to_vec(&Body {
        key: "RESEND_API_KEY",
        value: text,
        r#type: "sensitive",
        target: [target],
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(status: &str, body: &str, extra: &str) -> (Url, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!(
            "http://{}/fixture",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 2048];
            loop {
                let count = stream.read(&mut buf).unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..count]);
                if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                assert!(request.len() < 16384);
            }
            stream.write_all(response.as_bytes()).unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }

    fn fixture_client() -> Client {
        Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn http_boundary_filters_success_and_error_bodies() {
        let token = b"synthetic-api-token-for-test";
        let (url, server) = fixture(
            "200 OK",
            r#"{"projects":[{"id":"prj_test","name":"Demo","env":[{"value":"secret-fixture"}]}]}"#,
            "",
        );
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "projects",
            },
            token,
            None,
        )
        .await
        .unwrap();
        let request = server.join().unwrap();
        assert!(request.contains("authorization: Bearer synthetic-api-token-for-test"));
        assert!(
            !request
                .lines()
                .next()
                .unwrap()
                .contains("synthetic-api-token")
        );
        assert!(!result.to_string().contains("secret-fixture"));
        let (url, server) = fixture(
            "403 Forbidden",
            r#"{"message":"synthetic-api-token-for-test"}"#,
            "",
        );
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "projects",
            },
            token,
            None,
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["http_status"], 403);
        assert!(!result.to_string().contains("synthetic-api-token"));
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_writes_do_not_echo_secrets() {
        let token = b"synthetic-api-token-for-test";
        let (url, server) = fixture("302 Found", "", "Location: http://127.0.0.1:9/leak\r\n");
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "projects",
            },
            token,
            None,
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["http_status"], 302);
        let (url, server) = fixture(
            "200 OK",
            r#"{"created":{"key":"RESEND_API_KEY","value":"synthetic-sending-key-test"},"failed":[]}"#,
            "",
        );
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "copy_resend_key",
            },
            token,
            Some(secret_body("production", b"synthetic-sending-key-test").unwrap()),
        )
        .await
        .unwrap();
        let request = server.join().unwrap();
        assert!(request.starts_with("POST "));
        assert!(request.contains("synthetic-sending-key-test"));
        assert_eq!(result["status"], "accepted");
        assert!(!result.to_string().contains("synthetic-sending-key-test"));
    }

    #[tokio::test]
    async fn lost_write_response_is_uncertain_and_production_client_requires_https() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = Url::parse(&format!("http://{}/closed", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url: url.clone(),
                kind: "copy_resend_key",
            },
            b"synthetic-api-token-for-test",
            Some(vec![]),
        )
        .await
        .unwrap();
        assert_eq!(result["status"], "uncertain");
        assert_eq!(result["retry_write"], false);
        assert!(
            client()
                .unwrap()
                .get(url)
                .send()
                .await
                .unwrap_err()
                .is_builder()
        );
    }

    #[tokio::test]
    async fn http_success_with_failed_update_is_not_reported_as_success() {
        let (url, server) = fixture(
            "201 Created",
            r#"{"failed":[{"error":{"value":"secret-value","message":"secret-value"}}]}"#,
            "",
        );
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "copy_resend_key",
            },
            b"synthetic-api-token-for-test",
            Some(vec![]),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["status"], "uncertain");
        assert!(!result.to_string().contains("secret-value"));
    }
    #[tokio::test]
    async fn malformed_write_response_is_uncertain() {
        let (url, server) = fixture("200 OK", "secret-value-not-json", "");
        let result = execute(
            &fixture_client(),
            &Plan {
                slot: "vercel",
                url,
                kind: "copy_resend_key",
            },
            b"synthetic-api-token-for-test",
            Some(vec![]),
        )
        .await
        .unwrap();
        server.join().unwrap();
        assert_eq!(result["status"], "uncertain");
        assert_eq!(result["retry_write"], false);
        assert!(!result.to_string().contains("secret-value"));
    }
    #[test]
    fn rejects_endpoint_injection_and_unknown_inputs() {
        for project in [
            "../x",
            "https://attacker.test",
            "x?decrypt=true",
            "x/y",
            "x%2Fy",
        ] {
            assert!(
                plan(Read::VercelEnv {
                    project: project.into(),
                    team_id: None
                })
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<Read>(
                json!({"action":"vercel_env","project":"safe","decrypt":true})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<Read>(json!({"action":"resend_domains","token":"anything"}))
                .is_err()
        );
    }
    #[test]
    fn writes_need_exact_scope_and_owner_request() {
        let mut args = CopyResendKey {
            project: "prj_test".into(),
            team_id: None,
            target: "production".into(),
            owner_requested: false,
        };
        assert!(copy_plan(&args).is_err());
        args.owner_requested = true;
        assert_eq!(
            copy_plan(&args).unwrap().url.host_str(),
            Some("api.vercel.com")
        );
        args.target = "development".into();
        assert!(copy_plan(&args).is_err());
    }
    #[test]
    fn projects_omit_nested_secrets_and_redact_reflected_auth() {
        let token = b"test-credential-not-real";
        let out=project("projects",&json!({"projects":[{"id":"prj_test","name":"test-credential-not-real","env":[{"value":"other-secret"}],"secret":"other-secret"}]}),token).unwrap().to_string();
        assert!(!out.contains("test-credential-not-real"));
        assert!(!out.contains("other-secret"));
        assert!(out.contains("REDACTED"));
    }
    #[test]
    fn env_metadata_never_returns_values() {
        let out=project("env",&json!({"envs":[{"key":"RESEND_API_KEY","value":"secret-value","legacyValue":"secret-value","vsmValue":"secret-value","comment":"secret-value","target":["production"],"type":"sensitive"}]}),b"test-credential-not-real").unwrap().to_string();
        assert!(!out.contains("secret-value"));
        assert!(out.contains("RESEND_API_KEY"));
    }
    #[test]
    fn bearer_header_debug_is_sensitive_and_body_is_fixed() {
        let header = auth_header(b"test-credential-not-real").unwrap();
        assert!(!format!("{header:?}").contains("test-credential-not-real"));
        let body: Value =
            serde_json::from_slice(&secret_body("production", b"fake-sending-token-only").unwrap())
                .unwrap();
        assert_eq!(body["key"], "RESEND_API_KEY");
        assert_eq!(body["type"], "sensitive");
    }
}
