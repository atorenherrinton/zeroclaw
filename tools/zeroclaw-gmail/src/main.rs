use anyhow::{Result, bail};
use zeroclaw_gmail::{api::Gmail, auth, protocol, tools};

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
        ["schema"] => {
            println!("{}", serde_json::json!({"tools":tools::definitions()}));
            Ok(())
        }
        ["doctor"] | ["doctor", "--interactive"] => {
            let interactive = args.len() == 2;
            if interactive {
                use std::io::IsTerminal;
                anyhow::ensure!(
                    std::io::stdin().is_terminal(),
                    "interactive doctor requires an owner-operated terminal"
                );
            }
            let (account, _) = auth::configuration()?;
            let _api = if interactive {
                Gmail::connect_interactive(&account).await?
            } else {
                Gmail::connect(&account).await?
            };
            println!(
                "{}",
                serde_json::json!({"read_only":true,"account_matches":true,"gmail_access":true,"draft_only_transport":true})
            );
            Ok(())
        }
        _ => bail!("usage: zeroclaw-gmail mcp|schema|doctor [--interactive]"),
    }
}
