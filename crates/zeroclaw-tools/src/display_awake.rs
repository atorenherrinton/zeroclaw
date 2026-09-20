//! Per-call display leases. No timer task, process, or cross-call state.

use std::time::Duration;

use zeroclaw_config::schema::{McpServerConfig, McpTransport};

/// Configured local route and original tool name are authoritative, never a
/// server-provided description/annotation or a substring of arbitrary code.
pub(crate) fn mcp_needs_display(config: &McpServerConfig, tool: &str) -> bool {
    config.transport == McpTransport::Stdio
        && matches!(
            (config.name.as_str(), tool),
            ("cua_repl", "js")
                | ("public_browser" | "safari_browser", "browse" | "interact")
                | ("auth_browser", "browse" | "interact" | "login")
        )
}

// Absolute safety ceiling, independent of user-configured tool/sidecar timeouts.
// This creates the wake policy; call and parent deadlines may only shorten it.
const MAX_LEASE: Duration = Duration::from_secs(5 * 60);

/// The call deadline is canonical; never extend it when acquiring a lease.
pub(crate) fn acquire(budget: Duration) -> anyhow::Result<impl Send> {
    let budget = budget.min(MAX_LEASE);
    let budget = zeroclaw_api::deadline::current().map_or(budget, |deadline| {
        budget.min(deadline.saturating_duration_since(tokio::time::Instant::now()))
    });
    if budget.is_zero() {
        return Err(zeroclaw_api::deadline::DeadlineExceeded {
            phase: zeroclaw_api::deadline::Phase::Tool,
            started: false,
        }
        .into());
    }
    // Unit tests must never manipulate the physical display. The explicit
    // ignored native test below bypasses this adapter on a dedicated test Mac.
    #[cfg(test)]
    return testing::acquire(budget);
    #[cfg(not(test))]
    native::acquire(budget)
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{
        ffi::{CStr, c_char, c_void},
        ptr,
    };

    type CFStringRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            allocator: *const c_void,
            text: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(value: *const c_void);
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithDescription(
            kind: CFStringRef,
            name: CFStringRef,
            details: CFStringRef,
            reason: CFStringRef,
            bundle: CFStringRef,
            timeout: f64,
            action: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
        #[cfg(test)]
        fn IOPMAssertionCopyProperties(id: u32) -> *const c_void;
    }

    struct CfValue(CFStringRef);
    impl CfValue {
        fn new(text: &CStr) -> anyhow::Result<Self> {
            // SAFETY: valid NUL-terminated UTF-8 and the default CF allocator.
            let value =
                unsafe { CFStringCreateWithCString(ptr::null(), text.as_ptr(), 0x0800_0100) };
            anyhow::ensure!(
                !value.is_null(),
                crate::i18n::get_required_tool_string("tool-display-awake-allocation-error")
            );
            Ok(Self(value))
        }
    }
    impl Drop for CfValue {
        fn drop(&mut self) {
            // SAFETY: owns exactly one retained CF object.
            unsafe { CFRelease(self.0) };
        }
    }

    pub(super) struct Lease(Vec<u32>);
    impl Drop for Lease {
        fn drop(&mut self) {
            for id in self.0.drain(..) {
                // SAFETY: each ID was returned by IOKit to this lease. The OS
                // may already have released it at its hard timeout.
                let status = unsafe { IOPMAssertionRelease(id) };
                if status != 0 {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Reject)
                            .with_attrs(serde_json::json!({"status": status})),
                        "display_awake: assertion release failed or assertion already expired"
                    );
                }
            }
        }
    }

    pub(super) fn acquire(budget: Duration) -> anyhow::Result<Lease> {
        let name = CfValue::new(c"ZeroClaw active computer/browser tool")?;
        let action = CfValue::new(c"TimeoutActionRelease")?;
        let mut lease = Lease(Vec::with_capacity(2));
        // Like Apple's caffeinate -du, create both assertions with timeout-release
        // atomically. DeclareUserActivity followed by property updates would leave
        // a window where the wake assertion has only the user's idle timeout.
        // Each call gets distinct IDs; no CF references escape this function.
        for kind in [c"PreventUserIdleDisplaySleep", c"UserIsActive"] {
            let kind = CfValue::new(kind)?;
            let mut id = 0;
            // SAFETY: owned CF strings and valid output pointer. The caller
            // supplies a positive, finite timeout; zero would mean no expiration.
            check_status(unsafe {
                IOPMAssertionCreateWithDescription(
                    kind.0,
                    name.0,
                    ptr::null(),
                    ptr::null(),
                    ptr::null(),
                    budget.as_secs_f64(),
                    action.0,
                    &mut id,
                )
            })?;
            // If the next allocation/assertion fails, Drop releases this ID.
            lease.0.push(id);
        }
        Ok(lease)
    }

    fn check_status(status: i32) -> anyhow::Result<()> {
        anyhow::ensure!(
            status == 0,
            crate::i18n::get_required_tool_string_with_args(
                "tool-display-awake-native-error",
                &[("status", &status.to_string())]
            )
        );
        Ok(())
    }

    #[cfg(test)]
    #[test]
    #[ignore = "Changes the physical display; run only on an idle dedicated test Mac"]
    fn native_display_lease_manual() {
        let lease = acquire(Duration::from_secs(5)).unwrap();
        assert_eq!(lease.0.len(), 2);
        let ids = lease.0.clone();
        drop(lease);
        for id in ids {
            // SAFETY: IOKit accepts expired IDs and returns NULL.
            let properties = unsafe { IOPMAssertionCopyProperties(id) };
            if !properties.is_null() {
                drop(CfValue(properties));
                panic!("display assertion remains active");
            }
        }
    }

    #[cfg(test)]
    #[test]
    #[ignore = "Changes the physical display; run only on an idle dedicated test Mac"]
    fn native_display_timeout_manual() {
        let lease = acquire(Duration::from_millis(100)).unwrap();
        std::thread::sleep(Duration::from_secs(2));
        for &id in &lease.0 {
            // SAFETY: query only; NULL proves release without dropping the lease.
            let properties = unsafe { IOPMAssertionCopyProperties(id) };
            if !properties.is_null() {
                drop(CfValue(properties));
                panic!("display assertion remains active");
            }
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
mod native {
    use super::*;
    pub(super) fn acquire(_budget: Duration) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[derive(Default)]
    pub(crate) struct State {
        pub budgets: Vec<Duration>,
        pub active: usize,
        pub fail: bool,
    }
    tokio::task_local! { pub(crate) static STATE: Arc<Mutex<State>>; }
    pub(crate) struct Lease(Option<Arc<Mutex<State>>>);
    impl Drop for Lease {
        fn drop(&mut self) {
            if let Some(state) = &self.0 {
                state.lock().unwrap().active -= 1;
            }
        }
    }
    pub(super) fn acquire(budget: Duration) -> anyhow::Result<Lease> {
        let state = STATE.try_with(Arc::clone).ok();
        if let Some(state) = &state {
            let mut state = state.lock().unwrap();
            anyhow::ensure!(!state.fail, "injected display assertion failure");
            state.budgets.push(budget);
            state.active += 1;
        }
        Ok(Lease(state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn display_budget_caps_arbitrary_config_and_rejects_zero() {
        use std::sync::{Arc, Mutex};
        use testing::{STATE, State};
        let state = Arc::new(Mutex::new(State::default()));
        STATE
            .scope(state.clone(), async {
                drop(acquire(Duration::MAX).unwrap());
                drop(acquire(Duration::from_secs(7)).unwrap());
                assert!(acquire(Duration::ZERO).is_err());
            })
            .await;
        let state = state.lock().unwrap();
        assert_eq!(state.budgets, [MAX_LEASE, Duration::from_secs(7)]);
        assert_eq!(state.active, 0);
    }

    #[test]
    fn display_route_allowlist_is_exact_and_local() {
        for (name, tool, expected) in [
            ("cua_repl", "js", true),
            ("cua_repl", "js_reset", false),
            ("public_browser", "browse", true),
            ("safari_browser", "interact", true),
            ("auth_browser", "login", true),
            ("auth_browser", "browse", true),
            ("auth_browser", "interact", true),
            ("public_browser", "interact", true),
            ("safari_browser", "browse", true),
            ("auth_browser", "accounts", false),
            ("safari_browser", "close", false),
            ("public_browser", "close", false),
            ("auth_browser", "close", false),
            ("shell", "js", false),
            ("other_cua_repl", "js", false),
            ("public_browser", "unknown", false),
        ] {
            let mut config = McpServerConfig {
                name: name.into(),
                ..Default::default()
            };
            assert_eq!(mcp_needs_display(&config, tool), expected, "{name}/{tool}");
            for transport in [McpTransport::Http, McpTransport::Sse] {
                config.transport = transport;
                assert!(!mcp_needs_display(&config, tool));
            }
        }
    }
}
