use anyhow::{Result, ensure};
use std::path::Path;
use zeroclaw_typesafe_eval::{Dataset, collect, files, report};

fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("init") if args.len() == 2 => files::init(Path::new(&args[1]))?,
        Some("collect") if args.len() == 3 => {
            let dir = Path::new(&args[1]);
            files::private_dir(dir)?;
            let key = files::read_private(&dir.join("salt"))?;
            let data = collect(
                files::lines(files::open_private(Path::new(&args[2]))?),
                &key,
            )?;
            files::write_new(dir, "records.json", &serde_json::to_vec_pretty(&data)?)?;
        }
        Some("report") if (2..=4).contains(&args.len()) => {
            let dir = Path::new(&args[1]);
            files::private_dir(dir)?;
            let data: Dataset =
                serde_json::from_slice(&files::read_private(&dir.join("records.json"))?)?;
            let key = files::read_private(&dir.join("salt"))?;
            ensure!(
                data.experiment_id == zeroclaw_typesafe_eval::token(&key, "experiment", "")?,
                "experiment_key_mismatch"
            );
            let pairs = args
                .get(2)
                .map(|p| -> Result<report::Pairs> {
                    Ok(serde_json::from_slice(&files::read_private(Path::new(p))?)?)
                })
                .transpose()?;
            let labels = args
                .get(3)
                .map(|p| -> Result<report::Labels> {
                    Ok(serde_json::from_slice(&files::read_private(Path::new(p))?)?)
                })
                .transpose()?;
            let result = report::report(&data, pairs, labels)?;
            files::write_new(dir, "report.json", &serde_json::to_vec_pretty(&result)?)?;
        }
        _ => anyhow::bail!("usage"),
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        // Never echo parser errors, paths, or source data to shared stderr.
        let code = if let Some(io) = error.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::AlreadyExists => "output_already_exists",
                std::io::ErrorKind::NotFound => "input_or_parent_missing",
                std::io::ErrorKind::PermissionDenied => "permission_denied",
                _ => "file_io_error",
            }
        } else if error.downcast_ref::<serde_json::Error>().is_some() {
            "invalid_schema"
        } else {
            "invalid_protocol_permissions_or_bounds"
        };
        eprintln!(
            "{{\"ok\":false,\"error\":\"{code}\",\"usage\":\"init DIR | collect DIR TRACE_JSONL | report DIR [PAIRS_JSON [LABELS_JSON]]\"}}"
        );
        std::process::exit(1);
    }
    println!("{{\"ok\":true}}");
}
