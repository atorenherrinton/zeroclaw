use super::*;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

const PUBLIC: &str = "https://phone.example.invalid";
const PHONE: &str = "http://127.0.0.1:43335";
const BRIDGE: &str = "http://127.0.0.1:43336";

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("native");
        common::private_dir(&root).unwrap();
        Self {
            _directory: directory,
            root,
        }
    }

    fn bridge_dir(&self) -> PathBuf {
        self.root.join("extensions/google-push")
    }

    fn config_path(&self) -> PathBuf {
        self.bridge_dir().join("config.json")
    }

    fn config(&self) -> Value {
        json!({
            "Root": self.bridge_dir(),
            "PublicURL": PUBLIC,
            "Listen": "127.0.0.1:43336",
            "Upstream": PHONE,
            "Secret": "synthetic-not-used-or-emitted",
            "EnableRenewal": false,
        })
    }

    fn write(&self, config: &Value) {
        common::private_dir(&self.root.join("extensions")).unwrap();
        common::private_dir(&self.bridge_dir()).unwrap();
        common::atomic_private_write(&self.config_path(), &serde_json::to_vec(config).unwrap())
            .unwrap();
    }

    fn select(&self, address: &str) -> SafeResult<RouteKind> {
        select(&tunnels(address), &self.root, PUBLIC, 43335)
    }
}

fn tunnels(address: &str) -> Value {
    json!({"tunnels":[{"public_url":PUBLIC,"config":{"addr":address}}]})
}

#[test]
fn direct_route_does_not_require_or_create_bridge_configuration() {
    let fixture = Fixture::new();
    assert_eq!(fixture.select(PHONE).unwrap(), RouteKind::Direct);
    assert_eq!(fs::read_dir(&fixture.root).unwrap().count(), 0);
    fixture.write(&fixture.config());
    common::atomic_private_write(&fixture.config_path(), b"broken optional config").unwrap();
    assert_eq!(fixture.select(PHONE).unwrap(), RouteKind::Direct);
    assert_eq!(
        fs::read(fixture.config_path()).unwrap(),
        b"broken optional config"
    );
}

#[test]
fn exact_private_canonical_bridge_matches_without_changing_configuration() {
    let fixture = Fixture::new();
    fixture.write(&fixture.config());
    let bytes = fs::read(fixture.config_path()).unwrap();
    assert_eq!(fixture.select(BRIDGE).unwrap(), RouteKind::GooglePushBridge);
    assert_eq!(fs::read(fixture.config_path()).unwrap(), bytes);
    assert_eq!(
        serde_json::to_value(RouteKind::GooglePushBridge).unwrap(),
        "google_push_bridge"
    );
    assert_eq!(serde_json::to_value(RouteKind::Direct).unwrap(), "direct");
    // Listener ports are read from the one private bridge configuration, not
    // inferred from a healthy arbitrary proxy or hard-coded to this fixture.
    let mut config = fixture.config();
    config["Listen"] = json!("127.0.0.1:43337");
    fixture.write(&config);
    assert_eq!(
        fixture.select("http://127.0.0.1:43337").unwrap(),
        RouteKind::GooglePushBridge
    );
    assert!(fixture.select(BRIDGE).is_err());
}

#[test]
fn wrong_root_public_url_or_phone_upstream_never_authorizes_proxy() {
    let fixture = Fixture::new();
    let unrelated = fixture.root.join("unrelated");
    for (field, value) in [
        ("Root", unrelated.to_str().unwrap()),
        ("PublicURL", "https://other.example.invalid"),
        ("PublicURL", "https://phone.example.invalid.evil.invalid"),
        ("Upstream", "http://127.0.0.1:43334"),
        ("Upstream", "http://127.0.0.1:433350"),
        ("Upstream", "http://127.0.0.1:43335/other"),
        ("Upstream", "http://127.0.0.1:43335?target=other"),
        ("Upstream", "http://127.0.0.1:43335#other"),
        ("Upstream", "http://user@127.0.0.1:43335"),
        ("Upstream", "http://127.0.0.1.evil.invalid:43335"),
        ("Upstream", "http://localhost:43335"),
        ("Upstream", "https://127.0.0.1:43335"),
    ] {
        let mut config = fixture.config();
        config[field] = json!(value);
        fixture.write(&config);
        assert!(fixture.select(BRIDGE).is_err(), "{field}: {value}");
    }
}

#[test]
fn nonloopback_noncanonical_or_self_referential_bridge_listener_is_rejected() {
    let fixture = Fixture::new();
    for listen in [
        "0.0.0.0:43336",
        "192.0.2.1:43336",
        "localhost:43336",
        "127.0.0.2:43336",
        "127.0.0.1.evil.invalid:43336",
        "127.0.0.1:0",
        "127.0.0.1:43335",
        "127.0.0.1:043336",
        "127.0.0.1:43336/other",
        "user@127.0.0.1:43336",
        "http://127.0.0.1:43336",
        "[::1]:43336",
    ] {
        let mut config = fixture.config();
        config["Listen"] = json!(listen);
        fixture.write(&config);
        assert!(fixture.select(BRIDGE).is_err(), "{listen}");
    }
}

#[test]
fn arbitrary_or_prefix_matched_tunnel_targets_are_not_accepted() {
    let fixture = Fixture::new();
    fixture.write(&fixture.config());
    for address in [
        "http://127.0.0.1:43337",
        "http://127.0.0.1:433360",
        "http://127.0.0.1:43336/other",
        "http://127.0.0.1:43336?target=other",
        "http://127.0.0.1:43336#other",
        "http://user@127.0.0.1:43336",
        "http://127.0.0.1.evil.invalid:43336",
        "http://localhost:43336",
        "https://127.0.0.1:43336",
        "http://192.0.2.1:43336",
    ] {
        assert!(fixture.select(address).is_err(), "{address}");
    }
}

#[test]
fn malformed_missing_and_duplicate_public_tunnels_are_rejected() {
    let fixture = Fixture::new();
    for response in [
        json!({}),
        json!({"tunnels":null}),
        json!({"tunnels":[]}),
        json!({"tunnels":[{"public_url":"https://unrelated.example.invalid","config":{"addr":PHONE}}]}),
        json!({"tunnels":[{"public_url":PUBLIC,"config":{"addr":1}}]}),
        json!({"tunnels":[{"public_url":PUBLIC}]}),
        json!({"tunnels":[{"public_url":PUBLIC,"config":{"addr":PHONE}},{"public_url":PUBLIC,"config":{"addr":BRIDGE}}]}),
        json!({"tunnels":[{"public_url":PUBLIC,"config":{"addr":PHONE}},{"public_url":PUBLIC,"config":{"addr":PHONE}}]}),
    ] {
        assert!(select(&response, &fixture.root, PUBLIC, 43335).is_err());
    }
    assert!(select(&tunnels(PHONE), &fixture.root, PUBLIC, 0).is_err());
    let response = json!({"tunnels":[{"public_url":PUBLIC,"config":{"addr":PHONE}},{"public_url":"https://unrelated.example.invalid","config":{"addr":BRIDGE}}]});
    assert_eq!(
        select(&response, &fixture.root, PUBLIC, 43335).unwrap(),
        RouteKind::Direct
    );
}

#[test]
fn missing_or_invalid_bridge_config_fails_without_repair_or_directory_creation() {
    let fixture = Fixture::new();
    assert!(fixture.select(BRIDGE).is_err());
    assert!(!fixture.root.join("extensions").exists());
    fixture.write(&fixture.config());
    for bytes in [
        b"not-json".to_vec(),
        b"{}".to_vec(),
        br#"{"Root":false,"PublicURL":1,"Listen":null,"Upstream":[]}"#.to_vec(),
        vec![b' '; 2 * 1024 * 1024 + 1],
    ] {
        common::atomic_private_write(&fixture.config_path(), &bytes).unwrap();
        assert!(fixture.select(BRIDGE).is_err());
        assert_eq!(fs::read(fixture.config_path()).unwrap(), bytes);
    }
    fs::remove_file(fixture.config_path()).unwrap();
    assert!(fixture.select(BRIDGE).is_err());
    assert!(!fixture.config_path().exists());
}

#[test]
fn go_case_folded_aliases_and_duplicate_topology_fields_are_never_ignored() {
    let fixture = Fixture::new();
    let base = serde_json::to_string(&fixture.config()).unwrap();
    fixture.write(&fixture.config());
    for alias in [
        "root",
        "ROOT",
        "publicurl",
        "PublicUrl",
        "listen",
        "LISTEN",
        "upstream",
        "UpStream",
        "Liſten",
        "Upſtream",
        "Root",
        "PublicURL",
        "Listen",
        "Upstream",
    ] {
        let mut raw = base.trim_end_matches('}').to_owned();
        raw.push_str(&format!(",\"{alias}\":\"different-authority\"}}"));
        common::atomic_private_write(&fixture.config_path(), raw.as_bytes()).unwrap();
        assert!(fixture.select(BRIDGE).is_err(), "{alias}");
        assert_eq!(fs::read_to_string(fixture.config_path()).unwrap(), raw);
    }
}

#[test]
fn unsafe_bridge_file_and_each_private_directory_are_rejected_boundedly() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt, time::Instant};
    for kind in [
        "file_mode",
        "symlink",
        "directory",
        "fifo",
        "root_mode",
        "extensions_mode",
        "bridge_mode",
        "bridge_link",
        "extensions_link",
    ] {
        let fixture = Fixture::new();
        fixture.write(&fixture.config());
        let path = fixture.config_path();
        match kind {
            "file_mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            "symlink" => {
                let target = fixture.root.join("other.json");
                fs::rename(&path, &target).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "directory" => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
            "fifo" => {
                fs::remove_file(&path).unwrap();
                let path = CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            "root_mode" => {
                fs::set_permissions(&fixture.root, fs::Permissions::from_mode(0o755)).unwrap()
            }
            "extensions_mode" => fs::set_permissions(
                fixture.root.join("extensions"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap(),
            "bridge_mode" => {
                fs::set_permissions(fixture.bridge_dir(), fs::Permissions::from_mode(0o755))
                    .unwrap()
            }
            "bridge_link" | "extensions_link" => {
                let source = if kind == "bridge_link" {
                    fixture.bridge_dir()
                } else {
                    fixture.root.join("extensions")
                };
                let target = fixture.root.join("other-directory");
                fs::rename(&source, &target).unwrap();
                std::os::unix::fs::symlink(target, source).unwrap();
            }
            _ => unreachable!(),
        }
        let start = Instant::now();
        assert!(fixture.select(BRIDGE).is_err(), "{kind}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "{kind}"
        );
    }
}
