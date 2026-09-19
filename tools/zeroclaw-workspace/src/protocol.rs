//! Bounded newline-delimited stdio MCP. Notifications never invoke tools.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_LINE: usize = 512 * 1024;
pub async fn read_line(input: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        let available = input.fill_buf().await?;
        if available.is_empty() {
            return Ok(line);
        }
        let n = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |n| n + 1);
        ensure!(line.len() + n <= MAX_LINE, "MCP input exceeds bound");
        let done = available[n - 1] == b'\n';
        line.extend_from_slice(&available[..n]);
        input.consume(n);
        if done {
            return Ok(line);
        }
    }
}
fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
pub async fn response(request: Value) -> Option<Value> {
    let id = request.get("id").cloned();
    if request["jsonrpc"] != "2.0"
        || !request["method"].is_string()
        || id
            .as_ref()
            .is_some_and(|v| !(v.is_string() || v.is_number() || v.is_null()))
    {
        return Some(error(Value::Null, -32600, "Invalid Request"));
    }
    let id = id?;
    let result = match request["method"].as_str()? {
        "initialize" => {
            let version = request["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2024-11-05");
            let version = if ["2024-11-05", "2025-03-26", "2025-06-18"].contains(&version) {
                version
            } else {
                "2024-11-05"
            };
            json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"zeroclaw-workspace","version":env!("CARGO_PKG_VERSION")}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":crate::tools::definitions()}),
        "tools/call" => {
            let Some(name) = request["params"]["name"].as_str() else {
                return Some(error(id, -32602, "tool name required"));
            };
            let args = request["params"]
                .get("arguments")
                .cloned()
                .unwrap_or(json!({}));
            match crate::tools::call(name, &args).await {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(e) => json!({"content":[{"type":"text","text":e.to_string()}],"isError":true}),
            }
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}
pub async fn serve(
    input: &mut (impl AsyncBufRead + Unpin),
    output: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    loop {
        let line = read_line(input).await?;
        if line.is_empty() {
            break;
        }
        let reply = match serde_json::from_slice(&line) {
            Ok(request) => response(request).await,
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        };
        if let Some(reply) = reply {
            output.write_all(format!("{reply}\n").as_bytes()).await?;
            output.flush().await?;
        }
    }
    Ok(())
}
