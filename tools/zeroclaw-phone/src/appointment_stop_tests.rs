// Included in the existing workflow fixture module: the scheduler, journal,
// command owner and canonical state reader are real; external services are not.

fn native(fixture: &Fixture) -> PathBuf {
    common::native_dir(&fixture.root).unwrap()
}

fn policy(fixture: &Fixture, enabled: bool) {
    common::atomic_private_write(
        &native(fixture).join("config.toml"),
        format!(
            "[security.estop]\nenabled = {enabled}\nstate_file = 'isolated-estop-state.json'\nrequire_otp_to_resume = false\n"
        )
        .as_bytes(),
    )
    .unwrap();
}

fn state_path(fixture: &Fixture) -> PathBuf {
    native(fixture).join("isolated-estop-state.json")
}

fn write_state(path: &Path, state: Value) {
    common::atomic_private_write(path, &serde_json::to_vec(&state).unwrap()).unwrap();
}

fn proposal_count(fixture: &Fixture) -> i64 {
    database(&fixture.root)
        .unwrap()
        .query_row("SELECT count(*) FROM appointment_proposals", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn fresh_call(fixture: &Fixture, services: Arc<FakeServices>) -> InboundScheduler {
    let sid = format!("CA{}", "3".repeat(32));
    database(&fixture.root).unwrap().execute(
        "INSERT INTO calls(call_sid,account_sid,from_candidate,consent,consent_token,created_ms,phase) SELECT ?1,account_sid,from_candidate,consent,'another-synthetic-nonce',created_ms,phase FROM calls WHERE call_sid=?2",
        params![sid, fixture.sid],
    ).unwrap();
    InboundScheduler {
        root: fixture.root.clone(),
        call_sid: sid,
        services,
    }
}

#[tokio::test]
async fn estop_preengaged_layers_deny_before_lookup_but_unrelated_freeze_does_not() {
    for state in [
        json!({"kill_all": true}),
        json!({"network_kill": true}),
        json!({"blocked_domains": ["example.invalid"]}),
        json!({"frozen_tools": [crate::appointments::TOOL_NAME]}),
        json!({"frozen_tools": ["google_write__calendar_mutate"]}),
        json!({"frozen_tools": ["GOOGLE_WRITE__CALENDAR_MUTATE"]}),
    ] {
        let fixture = Fixture::new();
        policy(&fixture, true);
        write_state(&state_path(&fixture), state);
        assert!(InboundScheduler::for_call(&fixture.root, &fixture.sid).is_err());
        let fake = Arc::new(FakeServices::new());
        let result = fixture
            .scheduler(fake.clone())
            .schedule(fixture.request())
            .await;
        assert_eq!(result["status"], "message_only");
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0);
        assert_eq!(fake.finds.load(Ordering::SeqCst), 0);
        assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
        assert_eq!(proposal_count(&fixture), 0);
    }
    let fixture = Fixture::new();
    policy(&fixture, true);
    write_state(
        &state_path(&fixture),
        json!({"frozen_tools": ["unrelated_tool"]}),
    );
    let fake = Arc::new(FakeServices::new());
    assert_eq!(
        fixture
            .scheduler(fake.clone())
            .schedule(fixture.request())
            .await["status"],
        "tentative_hold_created"
    );
    assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_disabled_ignores_corrupt_state_and_enabled_fails_closed_without_rewriting() {
    for enabled in [false, true] {
        let fixture = Fixture::new();
        policy(&fixture, enabled);
        let path = state_path(&fixture);
        common::atomic_private_write(&path, b"not-json").unwrap();
        let fake = Arc::new(FakeServices::new());
        let result = fixture
            .scheduler(fake.clone())
            .schedule(fixture.request())
            .await;
        assert_eq!(
            result["status"],
            if enabled {
                "message_only"
            } else {
                "tentative_hold_created"
            }
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), usize::from(!enabled));
        assert_eq!(fake.writes.load(Ordering::SeqCst), usize::from(!enabled));
        assert_eq!(std::fs::read(path).unwrap(), b"not-json");
    }
}

#[tokio::test]
async fn estop_invalid_or_unreadable_current_policy_denies_without_external_work() {
    for kind in ["toml", "typed", "missing", "directory", "symlink", "fifo"] {
        let fixture = Fixture::new();
        let path = native(&fixture).join("config.toml");
        match kind {
            "toml" => common::atomic_private_write(&path, b"[security.estop").unwrap(),
            "typed" => {
                common::atomic_private_write(&path, b"[security.estop]\nenabled = 'yes'\n").unwrap()
            }
            "missing" => std::fs::remove_file(&path).unwrap(),
            "directory" => {
                std::fs::remove_file(&path).unwrap();
                std::fs::create_dir(&path).unwrap();
            }
            "symlink" => {
                let target = native(&fixture).join("other-policy.toml");
                common::atomic_private_write(&target, b"").unwrap();
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "fifo" => {
                use std::os::unix::ffi::OsStrExt;
                std::fs::remove_file(&path).unwrap();
                let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
            }
            _ => unreachable!(),
        }
        let fake = Arc::new(FakeServices::new());
        let started = std::time::Instant::now();
        assert_eq!(
            fixture
                .scheduler(fake.clone())
                .schedule(fixture.request())
                .await["status"],
            "message_only",
            "{kind}"
        );
        assert!(started.elapsed() < Duration::from_secs(1), "{kind}");
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0, "{kind}");
        assert_eq!(fake.writes.load(Ordering::SeqCst), 0, "{kind}");
        assert_eq!(proposal_count(&fixture), 0, "{kind}");
    }
}

pub(super) async fn pending_lookup_command(path: &Path) -> SafeResult<()> {
    let mut command = tokio::process::Command::new("/bin/sh");
    // Same harmless descendant fixture as appointment_commands. The actual
    // command owner handles cancellation; no test-owned replacement reaper.
    command.args([
        "-c",
        "/bin/sleep 30 >/dev/null 2>&1 & descendant=$!; printf '%s %s' \"$$\" \"$descendant\" > \"$1\"; exec /bin/sleep 30",
        "appointment_stop_fixture",
    ]).arg(path);
    crate::appointment_commands::run(command, None, 1024, Duration::from_secs(30)).await?;
    Ok(())
}

async fn fixture_pids(path: &Path) -> (i32, i32) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(path).await {
                let pids: Vec<i32> = text
                    .split_whitespace()
                    .filter_map(|value| value.parse().ok())
                    .collect();
                if pids.len() == 2 {
                    return (pids[0], pids[1]);
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("real owned lookup process must become ready")
}

async fn assert_group_reaped(leader: i32, descendant: i32) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if [leader, descendant, -leader].into_iter().all(|pid| {
                (unsafe { libc::kill(pid, 0) }) == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("cancelled lookup leader and descendants must be gone");
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[tokio::test]
async fn estop_pending_lookup_cancels_real_owned_group_and_preserves_unrelated_child() {
    let fixture = Fixture::new();
    policy(&fixture, true);
    write_state(&state_path(&fixture), json!({}));
    let pid_path = native(&fixture).join("lookup-pids");
    let mut fake = FakeServices::new();
    fake.pending_lookup_process = Some(pid_path.clone());
    let fake = Arc::new(fake);
    let scheduler = fixture.scheduler(fake.clone());
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let workflow = scheduler.schedule(fixture.request());
    tokio::pin!(workflow);
    let (leader, descendant) = tokio::select! {
        result = &mut workflow => panic!("lookup returned before stop: {result}"),
        pids = fixture_pids(&pid_path) => pids,
    };
    write_state(&state_path(&fixture), json!({"kill_all": true}));
    let result = tokio::time::timeout(Duration::from_secs(2), &mut workflow)
        .await
        .unwrap();
    assert_eq!(result["status"], "message_only");
    assert_eq!(fixture.state(), "unavailable");
    assert_eq!(fake.finds.load(Ordering::SeqCst), 0);
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
    assert_group_reaped(leader, descendant).await;
    assert!(unrelated.try_wait().unwrap().is_none());
    unrelated.start_kill().unwrap();
    tokio::time::timeout(Duration::from_secs(2), unrelated.wait())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn estop_final_prewrite_gate_prevents_writer_and_resume_never_replays_claim() {
    let fixture = Fixture::new();
    policy(&fixture, true);
    write_state(&state_path(&fixture), json!({}));
    let path = state_path(&fixture);
    let first = std::sync::atomic::AtomicBool::new(true);
    let mut fake = FakeServices::new();
    fake.before_write_action = Some(Box::new(move || {
        if first.swap(false, Ordering::SeqCst) {
            write_state(&path, json!({"kill_all": true}));
        }
    }));
    let fake = Arc::new(fake);
    let scheduler = fixture.scheduler(fake.clone());
    let request = fixture.request();
    assert_eq!(
        scheduler.schedule(request.clone()).await["status"],
        "outcome_uncertain"
    );
    assert_eq!(fixture.state(), "uncertain");
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
    write_state(&state_path(&fixture), json!({}));
    assert_eq!(
        scheduler.schedule(request.clone()).await["status"],
        "outcome_uncertain"
    );
    assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(fake.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        fresh_call(&fixture, fake.clone()).schedule(request).await["status"],
        "tentative_hold_created"
    );
    assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_pending_write_retains_uncertainty_and_cannot_replay_after_resume() {
    let fixture = Fixture::new();
    policy(&fixture, true);
    write_state(&state_path(&fixture), json!({}));
    let mut fake = FakeServices::new();
    fake.pending_write = true;
    let fake = Arc::new(fake);
    let scheduler = fixture.scheduler(fake.clone());
    let request = fixture.request();
    let workflow = scheduler.schedule(request.clone());
    tokio::pin!(workflow);
    tokio::select! {
        result = &mut workflow => panic!("write returned before stop: {result}"),
        _ = tokio::time::timeout(Duration::from_secs(2), async {
            while fake.writes.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }) => {},
    }
    assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state(), "writing");
    write_state(&state_path(&fixture), json!({"network_kill": true}));
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), &mut workflow)
            .await
            .unwrap()["status"],
        "outcome_uncertain"
    );
    assert_eq!(fixture.state(), "uncertain");
    write_state(&state_path(&fixture), json!({}));
    assert_eq!(
        scheduler.schedule(request).await["status"],
        "outcome_uncertain"
    );
    assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn estop_same_scheduler_observes_live_resume_without_reconstruction() {
    let fixture = Fixture::new();
    policy(&fixture, true);
    write_state(&state_path(&fixture), json!({"kill_all": true}));
    let fake = Arc::new(FakeServices::new());
    let scheduler = fixture.scheduler(fake.clone());
    let request = fixture.request();
    assert_eq!(
        scheduler.schedule(request.clone()).await["status"],
        "message_only"
    );
    assert_eq!(proposal_count(&fixture), 0);
    write_state(&state_path(&fixture), json!({}));
    assert_eq!(
        scheduler.schedule(request).await["status"],
        "tentative_hold_created"
    );
    assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
    assert_eq!(fake.writes.load(Ordering::SeqCst), 1);
}
