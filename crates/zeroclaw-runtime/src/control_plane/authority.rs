//! Runtime-authority guard — decides whether THIS process may reclaim a task.

use super::task_registry::TaskRecord;

pub fn is_authoritative(rec: &TaskRecord, current_boot_id: &str) -> bool {
    is_authoritative_with_pid_liveness(rec, current_boot_id, pid_is_alive)
}

fn is_authoritative_with_pid_liveness(
    rec: &TaskRecord,
    current_boot_id: &str,
    pid_is_alive: impl Fn(u32) -> bool,
) -> bool {
    // A different boot ID is not proof that its process exited: standalone
    // channels and a daemon can share the data directory. Never reclaim a live
    // owner. PID reuse can delay recovery; that is safer than duplicate effects.
    let _ = current_boot_id;
    !pid_is_alive(rec.owner_pid)
}

/// A zero signal only probes liveness; it never delivers a signal. Permission
/// denial and unknown errors are conservatively treated as a live owner.
fn pid_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return true;
        };
        // SAFETY: positive PID, signal 0, no pointers or process mutation.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::task_registry::{TaskKind, TaskStatus};

    fn rec(owner_pid: u32, owner_boot_id: &str) -> TaskRecord {
        TaskRecord {
            id: "t".into(),
            kind: TaskKind::Delegate,
            agent: "main".into(),
            status: TaskStatus::Running,
            owner_pid,
            owner_boot_id: owner_boot_id.into(),
            heartbeat_at: None,
            depth: 0,
            parent_id: None,
            originator_route: None,
            delivered: false,
            idem_key: None,
            principal_id: None,
            started_at: "2026-06-18T00:00:00Z".into(),
            finished_at: None,
        }
    }

    #[test]
    fn prior_boot_is_reclaimable() {
        // An exited process is reclaimable regardless of boot ID.
        assert!(is_authoritative(&rec(999_999, "boot-OLD"), "boot-NEW"));
    }

    #[test]
    fn different_boot_with_live_owner_is_never_reclaimed() {
        assert!(!is_authoritative(&rec(std::process::id(), "old"), "new"));
        assert!(!is_authoritative_with_pid_liveness(
            &rec(42, "old"),
            "new",
            |_| true
        ));
        assert!(is_authoritative_with_pid_liveness(
            &rec(42, "old"),
            "new",
            |_| false
        ));
    }

    #[test]
    fn unstamped_owner_is_reclaimable() {
        // Same boot but pid 0 (never stamped) ⇒ reclaimable.
        assert!(is_authoritative(&rec(0, "boot-NOW"), "boot-NOW"));
    }

    #[test]
    fn live_same_boot_pid_is_not_reclaimed() {
        // Our own live pid, same boot ⇒ must NOT be reclaimed.
        let me = std::process::id();
        assert!(!is_authoritative(&rec(me, "boot-NOW"), "boot-NOW"));
    }

    #[test]
    fn unstamped_boot_id_with_live_pid_is_not_reclaimed() {
        // Review finding #7: a record written before its boot_id is stamped (empty) and
        // owned by a LIVE pid must NOT be reaped via the boot-mismatch path — fail closed.
        let me = std::process::id();
        assert!(!is_authoritative(&rec(me, ""), "boot-NEW"));
    }

    #[test]
    fn unstamped_boot_id_reclaims_only_when_pid_liveness_says_dead() {
        assert!(!is_authoritative_with_pid_liveness(
            &rec(42, ""),
            "boot-NEW",
            |_| true,
        ));
        assert!(is_authoritative_with_pid_liveness(
            &rec(42, ""),
            "boot-NEW",
            |_| false,
        ));
    }
}
