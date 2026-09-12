//! Fixed-route launcher. No shell, caller-selected executable, or network operation.
use std::process::{Command, ExitCode};

// Capture the installing account at build time. Runtime callers cannot select
// another interpreter, renderer, workspace, or model cache.
const INSTALL_HOME: &str = env!("HOME");

fn valid_args(args: &[String]) -> bool {
    match args {
        [] => true,
        [command] => command == "status" || command == "mcp",
        [command, directory] => {
            (command == "validate" || command == "render")
                && std::path::Path::new(directory).is_absolute()
        }
        _ => false,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !valid_args(&args) {
        eprintln!(
            "usage: zeroclaw-youtube-studio [mcp | status | validate JOB_DIR | render JOB_DIR]"
        );
        return ExitCode::from(2);
    }
    let install = std::path::Path::new(INSTALL_HOME).join(".zeroclaw/extensions/youtube-studio");
    let status = Command::new(install.join("venv/bin/python"))
        .arg("-I")
        .arg(install.join("renderer.py"))
        .args(&args)
        .env_clear()
        .env("HOME", INSTALL_HOME)
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8")
        .env("HF_HOME", format!("{INSTALL_HOME}/.cache/huggingface"))
        .env("HF_HUB_OFFLINE", "1")
        .env("TRANSFORMERS_OFFLINE", "1")
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("TOKENIZERS_PARALLELISM", "false")
        .env(
            "PHONEMIZER_ESPEAK_LIBRARY",
            "/opt/homebrew/lib/libespeak-ng.dylib",
        )
        .env("OMP_NUM_THREADS", "4")
        .env("MKL_NUM_THREADS", "4")
        .status();
    match status {
        Ok(s) => ExitCode::from(s.code().unwrap_or(1).clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("unable to run installed YouTube draft renderer: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::valid_args;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).into()).collect()
    }
    #[test]
    fn only_documented_commands() {
        assert!(valid_args(&args(&["status"])));
        assert!(valid_args(&args(&["render", "/some/job"])));
        assert!(valid_args(&args(&["validate", "/some/job"])));
        assert!(valid_args(&args(&[])));
        assert!(valid_args(&args(&["mcp"])));
        for bad in [
            &["upload"][..],
            &["status", "/x"][..],
            &["render", "relative"][..],
            &["render", "/x", "--exec"][..],
        ] {
            assert!(!valid_args(&args(bad)));
        }
    }
}
