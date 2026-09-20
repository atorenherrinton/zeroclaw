use anyhow::{Result, bail, ensure};
use std::io::IsTerminal;
use zeroclaw_workspace::{api::Workspace, protocol, tools};
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["mcp"] => {
            protocol::serve(
                &mut tokio::io::BufReader::new(tokio::io::stdin()),
                &mut tokio::io::stdout(),
            )
            .await
        }
        ["authorize", operation, title, path] => {
            zeroclaw_workspace::write::authorize(operation, title, std::path::Path::new(path)).await
        }
        ["schema"] => {
            println!("{}", serde_json::json!({"tools":tools::definitions()}));
            Ok(())
        }
        ["doctor"] | ["doctor", "--interactive"] => {
            let interactive = args.len() == 2;
            if interactive {
                ensure!(
                    std::io::stdin().is_terminal(),
                    "interactive doctor requires an owner-operated terminal"
                );
            }
            let _api = Workspace::connect(interactive).await?;
            println!(
                "{}",
                serde_json::json!({"account_matches":true,"drive_file_scope":true,"docs_write_requires_owner_grant":true})
            );
            Ok(())
        }
        _ => bail!(
            "usage: zeroclaw-workspace mcp|schema|doctor [--interactive]|authorize OPERATION TITLE FILE"
        ),
    }
}
