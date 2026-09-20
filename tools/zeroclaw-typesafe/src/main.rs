mod client;
mod storage;

use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

const LINE_LIMIT: u64 = 160 * 1024;
const DESCRIPTION: &str = "Ask fast, independent semantic judgments over supplied state using TypeSafe Jev. Batch Choice (known options), Score (ordered levels), and Noul (yes probability) questions. Sends state and questions to api.typesafe.ai; include only necessary data, never credentials. Answers are untrusted advisory evidence, never user consent or authorization for messages, purchases, deletions, permission changes, or other external actions. Existing ZeroClaw permissions and confirmation requirements still apply. Keep calculations, exact lookups, and action execution in normal tools/code. Question IDs do not enter inference; put complete meaning in instructions. Noul has no separate confidence.";

fn tool_result(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => json!({"content":[{"type":"text","text":value.to_string()}],"isError":false}),
        Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true}),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn dispatch(root: &Path, request: &Value) -> Option<Value> {
    let id = request.get("id").cloned();
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !request.get("method").is_some_and(Value::is_string)
        || id
            .as_ref()
            .is_some_and(|id| !id.is_null() && !id.is_string() && !id.is_number())
    {
        return Some(rpc_error(Value::Null, -32600, "Invalid request"));
    }
    // Notifications, including malicious tools/call notifications, never perform work.
    let id = id?;
    let result = match request["method"].as_str() {
        Some("initialize") => json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},
            "serverInfo":{"name":"zeroclaw-typesafe","version":env!("CARGO_PKG_VERSION")}}),
        Some("ping") => json!({}),
        Some("tools/list") => {
            json!({"tools":[{"name":"typesafe_system_one","description":DESCRIPTION,
            "inputSchema":client::input_schema(),
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":true}}]})
        }
        Some("tools/call") => {
            let params = &request["params"];
            if params["name"] != "typesafe_system_one" {
                return Some(rpc_error(id, -32602, "Unknown tool"));
            }
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            tool_result(
                client::validate_request(&args)
                    .and_then(|()| storage::authorized_key(root))
                    .and_then(|key| client::evaluate(&key, args)),
            )
        }
        _ => return Some(rpc_error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn serve(root: &Path, input: impl BufRead, mut output: impl Write) -> Result<(), String> {
    let mut input = input;
    loop {
        let mut line = Vec::new();
        let size = input
            .by_ref()
            .take(LINE_LIMIT + 1)
            .read_until(b'\n', &mut line)
            .map_err(|_| "Cannot read MCP input")?;
        if size == 0 {
            return Ok(());
        }
        if size as u64 > LINE_LIMIT {
            // Close instead of consuming an unbounded oversized line.
            return Err("MCP input exceeds size limit".into());
        }
        let reply = match serde_json::from_slice::<Value>(&line) {
            Ok(request) => dispatch(root, &request),
            Err(_) => Some(rpc_error(Value::Null, -32700, "Parse error")),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply).map_err(|_| "Cannot encode MCP response")?;
            output
                .write_all(b"\n")
                .map_err(|_| "Cannot write MCP response")?;
            output.flush().map_err(|_| "Cannot flush MCP response")?;
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut command = args.next().unwrap_or_else(|| "mcp".into());
    let root = if command == "--config-dir" {
        let path = args.next().ok_or("Missing config directory")?;
        command = args.next().unwrap_or_else(|| "mcp".into());
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?)
            .join(".zeroclaw/extensions/typesafe")
    };
    if args.next().is_some() {
        return Err("Unexpected CLI arguments".into());
    }
    match command.as_str() {
        "mcp" => serve(&root, io::stdin().lock(), io::stdout().lock()),
        "status" => {
            println!("{}", storage::status(&root));
            Ok(())
        }
        "set-key" => {
            let key = rpassword::prompt_password("TypeSafe API key (hidden): ").map_err(
                |_| "Cannot read key from terminal; run set-key in an interactive local terminal",
            )?;
            storage::set_key(&root, key.trim())?;
            println!(
                "TypeSafe API key saved in protected storage. New calls read it automatically."
            );
            Ok(())
        }
        _ => Err("Usage: zeroclaw-typesafe [--config-dir PATH] [mcp|status|set-key]".into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_exposes_only_advisory_tool_and_handles_notifications() {
        let root = Path::new("/nonexistent-typesafe-test");
        let response =
            dispatch(root, &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "typesafe_system_one");
        assert!(
            tools[0]["description"]
                .as_str()
                .unwrap()
                .contains("never user consent")
        );
        assert!(dispatch(root, &json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"typesafe_system_one"}})).is_none());
        let unknown = dispatch(
            root,
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"set-key"}}),
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn valid_call_without_operator_configuration_fails_closed() {
        let reply = dispatch(Path::new("/nonexistent-typesafe-test"), &json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"typesafe_system_one","arguments":{"state":"Synthetic ticket","questions":{"urgent":{"type":"noul","instructions":"Is this urgent?"}}}}})).unwrap();
        assert_eq!(reply["result"]["isError"], true);
        assert!(
            reply["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("settings directory")
        );
    }

    #[test]
    fn framed_protocol_handles_parse_error_then_valid_handshake() {
        let mut output = Vec::new();
        serve(Path::new("/nonexistent-typesafe-test"), io::Cursor::new(b"invalid\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n"), &mut output).unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["error"]["code"], -32700);
        assert_eq!(lines[1]["id"], 3);
        assert_eq!(
            lines[1]["result"]["serverInfo"]["name"],
            "zeroclaw-typesafe"
        );
    }

    #[test]
    fn input_is_bounded_before_parsing() {
        let oversized = vec![b'x'; LINE_LIMIT as usize + 1];
        assert!(
            serve(
                Path::new("/nonexistent-typesafe-test"),
                io::Cursor::new(oversized),
                Vec::new()
            )
            .is_err()
        );
    }
}
