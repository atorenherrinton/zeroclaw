//! profiles.json is the canonical local configuration, re-read on each call.
//! It contains command/credential references only, never secret values.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};
use zeroize::Zeroizing;

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub credentials: BTreeMap<String, Source>,
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub service: String,
    pub account: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub executable: PathBuf,
    pub args: Vec<Argument>,
    #[serde(default)]
    pub params: BTreeMap<String, Parameter>,
    #[serde(default)]
    pub secret_env: BTreeMap<String, String>,
    pub secret_stdin: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub output: Output,
}
fn default_timeout() -> u64 {
    30
}
#[derive(Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Argument {
    Literal(String),
    Parameter { param: String },
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    #[serde(default)]
    pub choices: Vec<String>,
}
#[derive(Default, Deserialize, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Output {
    #[default]
    Status,
    Json {
        items: Option<String>,
        fields: BTreeMap<String, String>,
    },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub profile: String,
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub owner_requested: bool,
}
fn err(message: &'static str) -> anyhow::Error {
    anyhow::Error::msg(message)
}
pub fn config_path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or_else(|| err("Home directory unavailable"))?)
            .join(".zeroclaw/extensions/service-cli/profiles.json"),
    )
}
pub fn load() -> Result<Config> {
    let path = config_path()?;
    match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(_) => bail!("Could not read local profiles.json"),
        Ok(file) => {
            use std::os::unix::fs::MetadataExt;
            let metadata = file
                .metadata()
                .map_err(|_| err("Could not inspect profiles.json"))?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o022 != 0
            {
                bail!(
                    "profiles.json must be a regular file owned by you, with no group/other write access"
                );
            }
            use std::io::Read;
            let mut bytes = Vec::new();
            file.take(262145)
                .read_to_end(&mut bytes)
                .map_err(|_| err("Could not read profiles.json"))?;
            if bytes.len() > 262144 {
                bail!("profiles.json exceeds 256 KiB");
            }
            let config: Config = serde_json::from_slice(&bytes)
                .map_err(|_| err("Invalid profiles.json; inspect the local examples and guide"))?;
            validate(&config)?;
            Ok(config)
        }
    }
}
fn env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
fn text_ok(value: &str) -> bool {
    value.len() <= 4096 && !value.chars().any(char::is_control)
}
fn pointer_ok(value: &str) -> bool {
    value.len() <= 256 && (value.is_empty() || value.starts_with('/'))
}
pub fn validate(config: &Config) -> Result<()> {
    if config.profiles.len() > 200 || config.credentials.len() > 500 {
        bail!("Too many profiles or credential references");
    }
    for (name, source) in &config.credentials {
        crate::credentials::validate_slot(name)?;
        if source.service.is_empty()
            || source.account.is_empty()
            || !text_ok(&source.service)
            || !text_ok(&source.account)
        {
            bail!("A Keychain binding requires exact service and account metadata");
        }
    }
    for (name, p) in &config.profiles {
        crate::credentials::validate_slot(name)?;
        if !p.executable.is_absolute() || p.cwd.as_ref().is_some_and(|p| !p.is_absolute()) {
            bail!("Profile executable and optional cwd must be absolute paths");
        }
        if p.args.len() > 100
            || p.params.len() > 30
            || p.env.len() > 40
            || p.secret_env.len() > 20
            || !(1..=120).contains(&p.timeout_secs)
        {
            bail!("Profile exceeds argument, environment, or timeout limits");
        }
        let mut used = BTreeSet::new();
        for arg in &p.args {
            match arg {
                Argument::Literal(s) if !text_ok(s) => bail!("Invalid literal argument"),
                Argument::Parameter { param } => {
                    used.insert(param.clone());
                }
                _ => (),
            }
        }
        if used != p.params.keys().cloned().collect() {
            bail!("Every parameter must be declared and used in an argument");
        }
        for (name, param) in &p.params {
            crate::credentials::validate_slot(name)?;
            if param.choices.len() > 100 || param.choices.iter().any(|v| !parameter_value(v)) {
                bail!("Invalid parameter choices");
            }
        }
        for (key, val) in &p.env {
            if !env_name(key) || !text_ok(val) || p.secret_env.contains_key(key) {
                bail!("Invalid or conflicting public environment variable");
            }
        }
        for (key, slot) in &p.secret_env {
            if !env_name(key) {
                bail!("Invalid credential environment variable name");
            }
            crate::credentials::validate_slot(slot)?;
        }
        if let Some(slot) = &p.secret_stdin {
            crate::credentials::validate_slot(slot)?;
        }
        if let Output::Json { items, fields } = &p.output {
            if fields.is_empty()
                || fields.len() > 30
                || items.as_ref().is_some_and(|v| !pointer_ok(v))
            {
                bail!("Invalid JSON output projection");
            }
            for (label, pointer) in fields {
                crate::credentials::validate_slot(label)?;
                if !pointer_ok(pointer) {
                    bail!("Invalid JSON output field pointer");
                }
            }
        }
    }
    Ok(())
}
fn parameter_value(value: &str) -> bool {
    !value.is_empty() && text_ok(value) && !value.starts_with('-')
}
fn arguments(p: &Profile, input: &BTreeMap<String, String>) -> Result<Vec<String>> {
    if input.keys().collect::<Vec<_>>() != p.params.keys().collect::<Vec<_>>() {
        bail!("Parameters must exactly match the selected profile");
    }
    for (name, value) in input {
        if !parameter_value(value)
            || (!p.params[name].choices.is_empty() && !p.params[name].choices.contains(value))
        {
            bail!(
                "Invalid parameter value; flags and control characters are not accepted as parameters"
            );
        }
    }
    p.args
        .iter()
        .map(|arg| match arg {
            Argument::Literal(s) => Ok(s.clone()),
            Argument::Parameter { param } => input
                .get(param)
                .cloned()
                .ok_or_else(|| err("Missing profile parameter")),
        })
        .collect()
}
pub fn slots(config: &Config) -> BTreeSet<String> {
    let mut slots: BTreeSet<String> = crate::credentials::SLOTS
        .iter()
        .map(|s| (*s).into())
        .collect();
    slots.extend(config.credentials.keys().cloned());
    for p in config.profiles.values() {
        slots.extend(p.secret_env.values().cloned());
        slots.extend(p.secret_stdin.iter().cloned());
    }
    slots
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct List {
    pub profile: Option<String>,
    #[serde(default)]
    pub offset: usize,
}
pub fn list(config: &Config, args: List) -> Result<Value> {
    let selected: Vec<_> = if let Some(name) = args.profile {
        vec![(
            name.clone(),
            config
                .profiles
                .get(&name)
                .ok_or_else(|| err("Unknown profile"))?,
        )]
    } else {
        config
            .profiles
            .iter()
            .skip(args.offset)
            .take(20)
            .map(|(n, p)| (n.clone(), p))
            .collect()
    };
    let next = args.offset.saturating_add(selected.len());
    let result = json!({"profiles":selected.iter().map(|(name,p)| json!({
        "name":name,"executable":p.executable,"installed":p.executable.is_file(),
        "read_only":p.read_only,"params":p.params,"secret_env":p.secret_env,
        "secret_stdin":p.secret_stdin,"output":p.output,"timeout_secs":p.timeout_secs
    })).collect::<Vec<_>>(),"next_offset":if next<config.profiles.len(){Some(next)}else{None},"secret_values_returned":false});
    if serde_json::to_vec(&result)?.len() > 24576 {
        bail!("Profile details exceed budget; request one profile by name");
    }
    Ok(result)
}

async fn capture<R: AsyncRead + Unpin>(mut reader: R) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::new());
    let mut buffer = Zeroizing::new(vec![0; 8192]);
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| err("CLI output stream failed"))?;
        if count == 0 {
            return Ok(bytes);
        }
        if bytes.len() + count > 1024 * 1024 {
            bail!("CLI output exceeded limit");
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}
struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // Child starts a new process group. Kill remaining descendants on
        // timeout, cancellation, output overflow, and normal completion.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
fn metadata(
    raw: &Value,
    fields: &BTreeMap<String, String>,
    secrets: &[Zeroizing<Vec<u8>>],
) -> Value {
    let mut out = serde_json::Map::new();
    for (label, path) in fields {
        if let Some(value) = raw.pointer(path) {
            let value = if let Some(s) = value.as_str() {
                let mut s = s.to_string();
                for secret in secrets {
                    if let Ok(secret) = std::str::from_utf8(secret) {
                        s = s.replace(secret, "[REDACTED]");
                    }
                }
                Value::String(s.chars().take(512).collect())
            } else if value.is_null() || value.is_number() || value.is_boolean() {
                let encoded = value.to_string();
                if secrets
                    .iter()
                    .any(|secret| secret.as_slice() == encoded.as_bytes())
                {
                    Value::String("[REDACTED]".into())
                } else {
                    value.clone()
                }
            } else {
                continue;
            };
            out.insert(label.clone(), value);
        }
    }
    Value::Object(out)
}
fn project(output: &Output, bytes: &[u8], secrets: &[Zeroizing<Vec<u8>>]) -> Result<Value> {
    match output {
        Output::Status => Ok(Value::Null),
        Output::Json { items, fields } => {
            let raw: Value = serde_json::from_slice(bytes)
                .map_err(|_| err("CLI did not produce expected JSON; raw output omitted"))?;
            let result = if let Some(path) = items {
                let list = raw
                    .pointer(path)
                    .and_then(Value::as_array)
                    .ok_or_else(|| err("Unexpected CLI JSON array; raw output omitted"))?;
                json!({"items":list.iter().take(30).map(|v| metadata(v,fields,secrets)).collect::<Vec<_>>(),"truncated":list.len()>30})
            } else {
                metadata(&raw, fields, secrets)
            };
            if serde_json::to_vec(&result)?.len() > 24576 {
                bail!("Projected CLI output exceeded limit");
            }
            Ok(result)
        }
    }
}

pub async fn run(config: &Config, args: Run) -> Result<Value> {
    let p = config
        .profiles
        .get(&args.profile)
        .ok_or_else(|| err("Unknown profile; use profiles to discover configured commands"))?;
    if !p.read_only && !args.owner_requested {
        bail!("This profile can change state; an owner-authorized task is required");
    }
    let argv = arguments(p, &args.params)?;
    // Credential references resolve once from this call's freshly loaded config.
    let needed: BTreeSet<_> = p.secret_env.values().chain(p.secret_stdin.iter()).collect();
    let mut secrets = BTreeMap::new();
    for slot in needed {
        secrets.insert(
            slot.clone(),
            crate::credentials::load_from(slot, config, false)?,
        );
    }
    execute(p, &argv, &secrets).await
}
async fn execute(
    p: &Profile,
    argv: &[String],
    secrets: &BTreeMap<String, Zeroizing<Vec<u8>>>,
) -> Result<Value> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(&p.executable);
    command
        .args(argv)
        .env_clear()
        .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "en_US.UTF-8")
        .env("NO_COLOR", "1")
        .env(
            "HOME",
            std::env::var_os("HOME").ok_or_else(|| err("Home directory unavailable"))?,
        )
        .envs(&p.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.current_dir(p.cwd.as_deref().unwrap_or(Path::new("/")));
    command.as_std_mut().process_group(0);
    for (key, slot) in &p.secret_env {
        let bytes = secrets
            .get(slot)
            .ok_or_else(|| err("Missing profile credential"))?;
        command.env(
            key,
            std::str::from_utf8(bytes)
                .map_err(|_| err("Credential must be UTF-8 for CLI environment"))?,
        );
    }
    let stdin_secret = p
        .secret_stdin
        .as_ref()
        .map(|slot| {
            secrets
                .get(slot)
                .ok_or_else(|| err("Missing stdin credential"))
        })
        .transpose()?;
    let mut child = command
        .spawn()
        .map_err(|_| err("Could not start configured executable; check its local installation"))?;
    // Discard the Command's environment copies promptly after spawn.
    drop(command);
    let group = ProcessGroup(
        child
            .id()
            .ok_or_else(|| err("CLI process ID unavailable"))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| err("CLI stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| err("CLI stderr unavailable"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| err("CLI stdin unavailable"))?;
    let job = async {
        tokio::try_join!(
            async { child.wait().await.map_err(|_| err("CLI wait failed")) },
            capture(stdout),
            capture(stderr),
            async {
                if let Some(secret) = stdin_secret {
                    stdin
                        .write_all(secret)
                        .await
                        .map_err(|_| err("CLI input failed"))?;
                }
                drop(stdin);
                Ok(())
            }
        )
    };
    let outcome = tokio::time::timeout(Duration::from_secs(p.timeout_secs), job).await;
    drop(group);
    match outcome {
        Ok(Ok((status, stdout, _stderr, ()))) => {
            if !status.success() {
                return Ok(
                    json!({"status":if p.read_only {"failed"}else{"uncertain"},"exit_code":status.code(),"retry_write":false,"output_omitted":true}),
                );
            }
            let tokens: Vec<_> = secrets
                .values()
                .map(|s| Zeroizing::new(s.to_vec()))
                .collect();
            match project(&p.output, &stdout, &tokens) {
                Ok(data) => Ok(
                    json!({"status":"completed","exit_code":status.code(),"data":data,"untrusted_cli_data":true,"raw_output_returned":false,"retry_write":false,
                    "message":"CLI exited successfully; verify the intended remote effect separately."}),
                ),
                Err(_) => Ok(
                    json!({"status":if p.read_only {"output_unavailable"}else{"uncertain"},"exit_code":status.code(),"retry_write":false,"output_omitted":true}),
                ),
            }
        }
        _ => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(
                json!({"status":if p.read_only {"interrupted"}else{"uncertain"},"retry_write":false,"output_omitted":true,
                "message":"CLI stopped after a timeout or I/O limit; no raw output returned. Reconcile changes before retrying."}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(script: &str) -> Profile {
        serde_json::from_value(json!({"executable":"/usr/bin/python3","args":["-c",script],
            "secret_env":{"TEST_TOKEN":"test-token","TEST_SECOND":"test-second"},
            "secret_stdin":"test-stdin","read_only":true,
            "output":{"mode":"json","fields":{"ok":"/ok","echo":"/echo","argument":"/argument"}}}))
        .unwrap()
    }
    fn secrets() -> BTreeMap<String, Zeroizing<Vec<u8>>> {
        [
            ("test-token", "synthetic-env-credential"),
            ("test-second", "synthetic-second-credential"),
            ("test-stdin", "synthetic-stdin-credential"),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), Zeroizing::new(v.as_bytes().to_vec())))
        .collect()
    }
    fn argv(p: &Profile) -> Vec<String> {
        arguments(p, &BTreeMap::new()).unwrap()
    }
    #[tokio::test]
    async fn subprocess_gets_multiple_secrets_without_argument_or_output_leakage() {
        let mut p = fixture(
            r#"import os,sys,json
s=sys.stdin.read()
assert os.environ['TEST_TOKEN']=='synthetic-'+'env-credential'
assert os.environ['TEST_SECOND']=='synthetic-'+'second-credential'
assert s=='synthetic-'+'stdin-credential'
assert 'GH_DEBUG' not in os.environ
assert all(os.environ['TEST_TOKEN'] not in a for a in sys.argv[1:])
print(json.dumps({'ok':True,'echo':os.environ['TEST_TOKEN'],'argument':sys.argv[-1],'secret':s,'nested':{'token':s}}))
print(s,file=sys.stderr)
"#,
        );
        p.args.push(Argument::Parameter {
            param: "label".into(),
        });
        p.params
            .insert("label".into(), Parameter { choices: vec![] });
        let input = BTreeMap::from([("label".into(), "literal ; $(echo no-shell)".into())]);
        let passed_args = arguments(&p, &input).unwrap();
        for secret in secrets().values() {
            let text = std::str::from_utf8(secret).unwrap();
            assert!(passed_args.iter().all(|arg| !arg.contains(text)));
        }
        let result = execute(&p, &passed_args, &secrets()).await.unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["data"]["ok"], true);
        assert_eq!(result["data"]["echo"], "[REDACTED]");
        assert_eq!(result["data"]["argument"], "literal ; $(echo no-shell)");
        assert!(!result.to_string().contains("synthetic-"));
    }
    #[tokio::test]
    async fn status_mode_discards_all_output_and_failures_are_uncertain() {
        let mut p = fixture(
            "import os,sys; sys.stdin.read(); print(os.environ['TEST_TOKEN']); print(os.environ['TEST_SECOND'],file=sys.stderr)",
        );
        p.output = Output::Status;
        let result = execute(&p, &argv(&p), &secrets()).await.unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["data"], Value::Null);
        assert!(!result.to_string().contains("synthetic-"));
        p.read_only = false;
        p.args = vec![
            Argument::Literal("-c".into()),
            Argument::Literal(
                "import sys; sys.stdin.read(); print('synthetic-env-credential'); sys.exit(7)"
                    .into(),
            ),
        ];
        let result = execute(&p, &argv(&p), &secrets()).await.unwrap();
        assert_eq!(result["status"], "uncertain");
        assert_eq!(result["exit_code"], 7);
        assert!(!result.to_string().contains("synthetic-"));
    }
    #[tokio::test]
    async fn timeout_and_output_overflow_stop_processes() {
        let mut p = fixture("import sys,time; sys.stdin.read(); time.sleep(10)");
        p.timeout_secs = 1;
        p.read_only = false;
        let start = std::time::Instant::now();
        let result = execute(&p, &argv(&p), &secrets()).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(result["status"], "uncertain");
        p.args = vec![
            Argument::Literal("-c".into()),
            Argument::Literal("import sys; sys.stdin.read(); sys.stdout.write('x'*1100000)".into()),
        ];
        let result = execute(&p, &argv(&p), &secrets()).await.unwrap();
        assert_eq!(result["status"], "uncertain");
        assert!(result.to_string().len() < 1024);
    }
    #[tokio::test]
    async fn descendant_holding_pipes_is_killed_on_timeout() {
        // Parent exits immediately, grandchild retains output descriptors.
        let mut p = fixture(
            "import os,sys,time; sys.stdin.read(); pid=os.fork(); time.sleep(10) if pid==0 else None",
        );
        p.timeout_secs = 1;
        let start = std::time::Instant::now();
        let result = execute(&p, &argv(&p), &secrets()).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(result["status"], "interrupted");
    }
    #[tokio::test]
    async fn unconfirmed_writes_cannot_start_or_load_credentials() {
        let mut config = Config::default();
        let mut p = fixture("raise Exception('must never execute')");
        p.read_only = false;
        config.profiles.insert("write".into(), p);
        let args = Run {
            profile: "write".into(),
            params: BTreeMap::new(),
            owner_requested: false,
        };
        assert!(
            run(&config, args)
                .await
                .unwrap_err()
                .to_string()
                .contains("owner-authorized")
        );
    }
    #[test]
    fn shipped_examples_follow_the_profile_contract() {
        let config: Config =
            serde_json::from_str(include_str!("../profiles.example.json")).unwrap();
        validate(&config).unwrap();
        assert_eq!(config.profiles.len(), 3);
        assert!(slots(&config).contains("aws-secret-key"));
    }
    #[test]
    fn configuration_and_parameter_boundaries() {
        let mut config = Config::default();
        let mut p = fixture("pass");
        p.args.push(Argument::Parameter {
            param: "repo".into(),
        });
        p.params
            .insert("repo".into(), Parameter { choices: vec![] });
        assert!(arguments(&p, &BTreeMap::from([("repo".into(), "--debug".into())])).is_err());
        assert!(arguments(&p, &BTreeMap::new()).is_err());
        config.profiles.insert("gh-example".into(), p);
        validate(&config).unwrap();
        assert!(slots(&config).contains("test-second"));
        config.profiles.get_mut("gh-example").unwrap().executable = "python3".into();
        assert!(validate(&config).is_err());
        assert!(serde_json::from_value::<Config>(json!({"token":"do-not-accept"})).is_err());
        assert!(
            serde_json::from_value::<Run>(json!({"profile":"x","token":"do-not-accept"})).is_err()
        );
    }
    #[test]
    fn numeric_credentials_are_also_redacted() {
        let fields = BTreeMap::from([("number".into(), "/number".into())]);
        let output = metadata(
            &json!({"number":123456}),
            &fields,
            &[Zeroizing::new(b"123456".to_vec())],
        );
        assert_eq!(output["number"], "[REDACTED]");
    }
    #[test]
    fn projection_excludes_unknown_nested_and_error_fields() {
        let p = fixture("pass");
        let data=project(&p.output,br#"{"ok":true,"echo":"synthetic-env-credential","secret":"other","nested":{"password":"other"}}"#,&secrets().into_values().collect::<Vec<_>>()).unwrap();
        assert_eq!(data["echo"], "[REDACTED]");
        assert!(data.get("secret").is_none());
        assert!(data.get("nested").is_none());
        assert!(
            project(&p.output, b"malformed-secret-output", &[])
                .unwrap_err()
                .to_string()
                .contains("omitted")
        );
    }
}
