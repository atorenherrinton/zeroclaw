//! Fixed-purpose deterministic memory maintenance. No shell, model, or network.
use anyhow::{Context, Result};
use std::path::PathBuf;
use zeroclaw_config::schema::Config;
use zeroclaw_memory::SqliteMemory;

fn arguments() -> Result<(PathBuf, String, bool)> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        matches!(args.len(), 4 | 5)
            && args[0] == "--config-dir"
            && args[2] == "--agent"
            && (args.len() == 4 || args[4] == "--dry-run"),
        "invalid arguments"
    );
    let dir = PathBuf::from(&args[1]);
    anyhow::ensure!(
        dir.is_absolute()
            && !args[3].is_empty()
            && args[3]
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'),
        "invalid scope"
    );
    Ok((dir, args[3].clone(), args.len() == 5))
}

#[cfg(unix)]
fn require_private_file(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no preconditions and does not mutate process state.
    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.mode() & 0o077 == 0
            && meta.uid() == uid,
        "private file required"
    );
    Ok(())
}

#[cfg(not(unix))]
fn require_private_file(_path: &std::path::Path) -> Result<()> {
    anyhow::bail!("this maintenance helper requires Unix file ownership checks")
}

async fn run() -> Result<()> {
    let (dir, agent, dry_run) = arguments()?;
    anyhow::ensure!(
        dir.canonicalize()? == dir,
        "canonical installation path required"
    );
    for path in [dir.join("data"), dir.join("data/memory")] {
        let meta = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "native directory required"
        );
    }
    let config_path = dir.join("config.toml");
    require_private_file(&config_path)?;
    let mut config: Config =
        toml::from_str(&std::fs::read_to_string(&config_path)?).context("invalid native config")?;
    config.config_path = config_path;
    config.data_dir = dir.join("data");
    let agent_cfg = config.agents.get(&agent).context("agent missing")?;
    anyhow::ensure!(
        zeroclaw_memory::backend_kind_from_dotted(&config.memory.backend) == "sqlite"
            && matches!(
                agent_cfg.memory.backend,
                zeroclaw_config::multi_agent::MemoryBackendKind::Sqlite
            ),
        "SQLite required"
    );
    anyhow::ensure!(
        config.memory.promotion.enabled && config.memory.promotion.agent_aliases.contains(&agent),
        "promotion disabled"
    );
    require_private_file(&config.data_dir.join("memory/brain.db"))?;
    // No load_or_init or embedding-provider factory: never silently create an
    // installation, migrate legacy directories, or access a model/provider.
    let memory = SqliteMemory::new("sqlite", &config.data_dir)?
        .with_promotion_config(&config.memory.promotion, &config.memory.policy)?;
    let report = memory.promote_for_alias(&agent, dry_run).await?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

#[tokio::main]
async fn main() {
    // Whole-process deadline also bounds SQLite open/lock waits. SQLite rolls
    // back uncommitted transactions after termination; no children are spawned.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(30));
        std::process::exit(124);
    });
    if run().await.is_err() {
        eprintln!("memory_promotion_failed");
        std::process::exit(1);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use zeroclaw_api::ingress::TurnOrigin;
    use zeroclaw_api::memory_promotion::OWNER_RECALL_CONTEXT;
    use zeroclaw_memory::{Memory, MemoryCategory};

    /// Process-boundary test: first build this binary (`cargo build --bin
    /// zeroclaw-memory-promote`), then run its tests. No live config is read.
    #[tokio::test]
    async fn synthetic_process_dry_run_apply_idempotency_and_guard() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let mut config = Config {
            data_dir: dir.join("data"),
            config_path: dir.join("config.toml"),
            ..Config::default()
        };
        config.memory.promotion.enabled = true;
        config.memory.promotion.agent_aliases = vec!["owner".into()];
        config.agents.insert(
            "owner".into(),
            zeroclaw_config::schema::AliasedAgentConfig::default(),
        );
        let memory = SqliteMemory::new("sqlite", &config.data_dir)
            .unwrap()
            .with_promotion_config(&config.memory.promotion, &config.memory.policy)
            .unwrap();
        let id = memory.ensure_agent_uuid("owner").await.unwrap();
        let note = "I prefer Rust, SQLite, privacy, local tools and short notes.";
        memory
            .store_with_agent(
                "preference",
                note,
                MemoryCategory::Daily,
                None,
                None,
                None,
                Some(&id),
            )
            .await
            .unwrap();
        let context = |input: &str, turn: &str| {
            zeroclaw_memory::promotion::owner_context(
                &config.memory.promotion,
                Some("owner"),
                TurnOrigin::Interactive,
                "cli",
                turn,
                Some(input),
            )
        };
        OWNER_RECALL_CONTEXT
            .scope(
                context(note, "write"),
                memory.attest_explicit_note(Some(&id), "preference", note),
            )
            .await
            .unwrap();
        for (n, query) in ["Rust", "SQLite", "privacy", "local", "short"]
            .iter()
            .enumerate()
        {
            let entries = memory
                .recall_for_agents(&[&id], query, 5, None, None, None)
                .await
                .unwrap();
            OWNER_RECALL_CONTEXT
                .scope(
                    context(query, &format!("turn{n}")),
                    memory.record_recall_evidence(Some(&id), &entries),
                )
                .await
                .unwrap();
        }
        std::fs::write(&config.config_path, toml::to_string(&config).unwrap()).unwrap();
        for path in [
            &config.config_path,
            &config.data_dir.join("memory/brain.db"),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let executable = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("zeroclaw-memory-promote");
        assert!(
            executable.is_file(),
            "build the helper binary before running this process-boundary test"
        );
        let invoke = |agent: &str, dry: bool| {
            let mut command = std::process::Command::new(&executable);
            command
                .arg("--config-dir")
                .arg(&dir)
                .args(["--agent", agent]);
            if dry {
                command.arg("--dry-run");
            }
            command.output().unwrap()
        };
        let dry = invoke("owner", true);
        assert!(
            dry.status.success(),
            "{}",
            String::from_utf8_lossy(&dry.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
        assert_eq!(report["eligible"], 1);
        assert_eq!(report["promoted"], 0);
        let apply = invoke("owner", false);
        assert!(apply.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&apply.stdout).unwrap()["promoted"],
            1
        );
        assert!(!String::from_utf8_lossy(&apply.stdout).contains(note));
        let repeat = invoke("owner", false);
        assert!(repeat.status.success());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&repeat.stdout).unwrap()["promoted"],
            0
        );
        assert!(!invoke("foreign", false).status.success());
        assert_eq!(
            memory
                .get_for_agent("preference", &id)
                .await
                .unwrap()
                .unwrap()
                .content,
            note
        );
    }
}
