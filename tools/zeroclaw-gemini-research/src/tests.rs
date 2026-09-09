use super::*;
use std::net::TcpListener;

fn grounded(answer: &str) -> Value {
    json!({"status":"completed","steps":[
        {"type":"thought","summary":[{"text":"Never expose this"}]},
        {"type":"model_output","content":[{"type":"text","text":answer,"annotations":[{"type":"url_citation","title":"Official source","url":"https://example.org/official","start_index":0,"end_index":answer.len()}]}]},
        {"type":"google_search_call","arguments":{"queries":["public facts"]}},
        {"type":"google_search_result","result":[{"search_suggestions":"<style>irrelevant</style><a href=\"https://www.google.com/search?q=public+facts\">public facts</a>"}],"is_error":false}
    ],"usage":{"total_input_tokens":12,"total_output_tokens":34,"total_thought_tokens":56,"total_tool_use_tokens":78}})
}

#[test]
fn complete_answer_sources_suggestions_and_output_bound() {
    let result = render(&grounded("A complete answer. Uncertainty stays here.")).unwrap();
    assert!(result.contains("A complete answer. Uncertainty stays here."));
    assert!(result.contains("https://example.org/official"));
    assert!(result.contains("Google Search suggestions:"));
    assert!(!result.contains("Never expose this"));
    assert!(!result.contains("<style>"));
    assert!(serde_json::to_string(&result).unwrap().len() <= OUTPUT_LIMIT);
    // Escaping, not raw UTF-8 size, determines the history budget.
    assert!(render(&grounded(&"\"\n😀".repeat(1000))).is_err());
    assert!(render(&grounded(&"x".repeat(OUTPUT_LIMIT))).is_err());
}

#[test]
fn missing_evidence_incomplete_and_unsafe_links_fail_closed() {
    for kind in ["google_search_call", "google_search_result", "model_output"] {
        let mut value = grounded("Answer");
        value["steps"]
            .as_array_mut()
            .unwrap()
            .retain(|s| s["type"] != kind);
        assert!(render(&value).is_err());
    }
    let mut value = grounded("Answer");
    value["status"] = json!("incomplete");
    assert!(render(&value).is_err());
    value = grounded("Answer");
    value["steps"][1]["content"][0]["annotations"] = json!([]);
    assert!(render(&value).is_err());
    for url in [
        "javascript:alert(1)",
        "file:///private",
        "http://example.org",
        "https://user:password@example.org",
    ] {
        value = grounded("Answer");
        value["steps"][1]["content"][0]["annotations"][0]["url"] = json!(url);
        assert!(render(&value).is_err());
    }
}

#[test]
fn runtime_validation_excludes_extra_context_and_controls() {
    for args in [
        json!({}),
        json!({"question":" "}),
        json!({"question":"fine","api_key":"secret"}),
        json!({"question":"fine","model":"expensive"}),
        json!({"question":17}),
        json!({"question":"x".repeat(3001)}),
    ] {
        assert!(question(&args).is_err());
    }
    let args = json!({"question":"  public question  "});
    assert_eq!(question(&args).unwrap(), "public question");
    let body = payload("public question", "gemini-2.5-flash");
    assert_eq!(body["input"], "public question");
    assert_eq!(body["store"], false);
    assert_eq!(body["tools"], json!([{"type":"google_search"}]));
    assert!(body.get("previous_interaction_id").is_none());
}

#[test]
fn durable_limit_counts_failures_and_interrupted_attempts_across_processes() {
    let dir = tempfile::tempdir().unwrap();
    let settings = Settings {
        model: "gemini-2.5-flash".into(),
        daily_request_limit: 2,
    };
    let mut first = ledger(dir.path()).unwrap();
    let mut second = ledger(dir.path()).unwrap();
    let id = reserve(&mut first, &settings).unwrap();
    record(&first, id, Some(&grounded("Answer")), "ok").unwrap();
    reserve(&mut second, &settings).unwrap(); // Simulate an interrupted request.
    assert!(reserve(&mut first, &settings).is_err());
    let result = usage(&second, &settings).unwrap();
    assert_eq!(result["attempts_today_utc"], 2);
    assert_eq!(result["month_utc"]["unresolved_attempts"], 1);
    assert_eq!(result["month_utc"]["input_tokens"], 12);
    assert_eq!(result["month_utc"]["search_queries"], 1);
    let columns: Vec<String> = first
        .prepare("PRAGMA table_info(requests)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        !columns
            .iter()
            .any(|c| ["question", "prompt", "answer", "api_key"].contains(&c.as_str()))
    );
}

// Exercise the actual HTTP request, including redirects/errors and wire shape.
fn mock(status: &str, response: String) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let status = status.to_owned();
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let count = socket.read(&mut buf).unwrap();
            bytes.extend_from_slice(&buf[..count]);
            let request = String::from_utf8_lossy(&bytes);
            if let Some((headers, body)) = request.split_once("\r\n\r\n") {
                let length: usize = headers
                    .lines()
                    .find_map(|s| {
                        s.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .unwrap()
                    .parse()
                    .unwrap();
                if body.len() >= length {
                    break;
                }
            }
        }
        write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nLocation: http://127.0.0.1:1/never\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).unwrap();
        String::from_utf8(bytes).unwrap()
    });
    (format!("http://{address}/interactions"), worker)
}

#[test]
fn actual_transport_uses_fixed_payload_and_does_not_reflect_or_retry_errors() {
    let client = client().unwrap();
    let (url, worker) = mock("200 OK", grounded("Answer").to_string());
    let value = fetch(
        &client,
        &url,
        "synthetic-secret",
        &payload("public question", "gemini-2.5-flash"),
    )
    .unwrap();
    assert!(render(&value).is_ok());
    let wire = worker.join().unwrap();
    assert!(wire.starts_with("POST /interactions "));
    assert!(wire.contains("x-goog-api-key: synthetic-secret"));
    let body: Value = serde_json::from_str(wire.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body, payload("public question", "gemini-2.5-flash"));
    for status in [
        "302 Found",
        "429 Too Many Requests",
        "500 Internal Server Error",
    ] {
        let (url, worker) = mock(status, "synthetic-secret reflected by provider".into());
        let error = fetch(&client, &url, "synthetic-secret", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(!error.contains("synthetic-secret"));
        assert!(error.contains("not retried"));
        worker.join().unwrap();
    }
    let (url, worker) = mock("200 OK", "x".repeat(RESPONSE_LIMIT as usize + 1));
    assert!(fetch(&client, &url, "synthetic-secret", &json!({})).is_err());
    worker.join().unwrap();
}

#[test]
fn mcp_discovery_is_bounded_and_has_read_only_annotations() {
    let root = Path::new("/not-configured");
    assert!(respond(root, &json!({"method":"notifications/initialized"})).is_none());
    let result = respond(root, &json!({"id":1,"method":"tools/list"})).unwrap();
    let tools = result["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true);
    }
    assert!(result.to_string().len() < 8192);
}

#[test]
fn api_key_requires_private_file_and_errors_do_not_expose_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("api-key");
    std::fs::write(&path, "synthetic-secret").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(key(dir.path()).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(key(dir.path()).unwrap(), "synthetic-secret");
}
