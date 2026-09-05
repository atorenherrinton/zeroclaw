use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use zeroclaw_personal_ops::{Ops, call, schema, text};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("help");
    if mode == "help" {
        println!(
            "zeroclaw-personal-ops mcp CONFIG_DIR | install CONFIG_DIR GITHUB_ROOT | repair-routing CONFIG_DIR | enable-messages CONFIG_DIR | dispatch-messages CONFIG_DIR | tools"
        );
        return Ok(());
    }
    if mode == "tools" {
        println!("{}", schema());
        return Ok(());
    }
    let root = Path::new(args.get(2).context("CONFIG_DIR required")?);
    if mode == "dispatch-messages" {
        println!("{}", Ops::open(root)?.dispatch_due().await?);
        return Ok(());
    }
    if mode == "enable-messages" {
        return zeroclaw_personal_ops::install::enable_messages(root);
    }
    if mode == "repair-routing" {
        return zeroclaw_personal_ops::install::repair_routing(root);
    }
    if mode == "plan" || mode == "candidate" {
        let config: Value =
            toml::from_str::<toml::Value>(&std::fs::read_to_string(root.join("config.toml"))?)?
                .try_into()?;
        if mode == "candidate" {
            println!(
                "{}",
                toml::to_string_pretty(&zeroclaw_personal_ops::install::candidate(
                    &config,
                    root,
                    Path::new(args.get(3).context("GITHUB_ROOT required")?)
                )?)?
            );
            return Ok(());
        }
        println!(
            "{}",
            zeroclaw_personal_ops::install::patch(
                &config,
                root,
                Path::new(args.get(3).context("GITHUB_ROOT required")?)
            )?
        );
        return Ok(());
    }
    if mode == "install" {
        return zeroclaw_personal_ops::install::install(
            root,
            Path::new(args.get(3).context("GITHUB_ROOT required")?),
        );
    }
    if mode != "mcp" {
        bail!("unsupported command");
    }
    let ops = Ops::open(root)?;
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut out = tokio::io::stdout();
    loop {
        let mut bytes = Vec::new();
        let count = (&mut reader)
            .take(262145)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            break;
        }
        if bytes.len() > 262144 {
            bail!("MCP input exceeds 256 KiB");
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                out.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32700,\"message\":\"Invalid JSON\"}}\n").await?;
                continue;
            }
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        let result: Result<Value> = match request["method"].as_str().unwrap_or("") {
            "initialize" => Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"personal-ops","version":"0.1.0"}}),
            ),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools":schema()})),
            "tools/call" => {
                let params = &request["params"];
                let res = match text(params, "name", 64) {
                    Ok(name) => call(&ops, name, &params["arguments"]).await,
                    Err(e) => Err(e),
                };
                Ok(match res {
                    Ok(v) => {
                        json!({"content":[{"type":"text","text":v.to_string()}],"isError":false})
                    }
                    Err(e) => {
                        json!({"content":[{"type":"text","text":e.to_string()}],"isError":true})
                    }
                })
            }
            _ => Err(anyhow::Error::msg("Method not found")),
        };
        let response = match result {
            Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
            Err(_) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
            }
        };
        out.write_all(format!("{response}\n").as_bytes()).await?;
        out.flush().await?;
    }
    Ok(())
}
