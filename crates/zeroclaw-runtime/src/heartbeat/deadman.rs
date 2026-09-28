//! Parent-owned monitoring with a durable, single-attempt incident claim.
//! `data_dir/heartbeat/history.db` owns the watchdog baseline, tick generation,
//! and last delivery attempt; task history and live metrics are not substitutes.
//! `deadman_timeout_minutes = 0` mutes only this notification after config reload.
//! Muting/restarting does not re-arm a claimed incident; only a completed tick
//! does. Channel failures are uncertain and never retried automatically. This
//! favors avoiding duplicate alerts over guaranteed delivery after a crash.
//! Quiet hours and owner acknowledgements are not implemented here: heartbeat
//! has no canonical policy for either. Existing ordinary heartbeat tasks remain
//! independent of this notification, and recovery is logged internally only.
use super::store;
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::{future::Future, path::Path};
use tokio::time::Duration;

/// Neither future is detached. Dropping/reloading the parent drops both, and
/// a worker error cannot leave a watcher with stale configuration/metrics.
pub(crate) async fn supervise(
    worker: impl Future<Output = Result<()>>,
    watcher: impl Future<Output = Result<()>>,
) -> Result<()> {
    tokio::select! {
        result = worker => result,
        result = watcher => result,
    }
}

pub(crate) async fn watch<F, Fut>(data_dir: &Path, timeout_minutes: u32, deliver: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    watch_with_clock(data_dir, timeout_minutes, Utc::now, deliver).await
}

async fn watch_with_clock<F, Fut>(
    data_dir: &Path,
    timeout_minutes: u32,
    now: impl Fn() -> DateTime<Utc>,
    mut deliver: F,
) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if timeout_minutes == 0 {
        return std::future::pending().await;
    }
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let Some(sequence) = store::claim_deadman_alert(data_dir, now(), timeout_minutes)? else {
            continue;
        };
        let delivered = matches!(
            tokio::time::timeout(Duration::from_secs(30), deliver()).await,
            Ok(Ok(()))
        );
        store::finish_deadman_alert(data_dir, sequence, delivered)?;
        if !delivered {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown),
                "Deadman alert delivery unconfirmed; incident will not be retried"
            );
        }
    }
}

/// Persistence is required for re-arm. If it fails, the worker propagates the
/// error instead of pretending a volatile timestamp settled the incident.
pub(crate) fn completed_tick(data_dir: &Path) -> Result<()> {
    if store::record_completed_tick(data_dir, Utc::now())? {
        ::zeroclaw_log::record!(
            INFO,
            ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
            "Heartbeat recovered; deadman alert re-armed after completed tick"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn timeout_after_possible_delivery_is_not_retried_and_parent_drop_cancels() {
        let tmp = tempfile::tempdir().unwrap();
        let base = Utc::now();
        store::start_deadman(tmp.path(), base).unwrap();
        let start = tokio::time::Instant::now();
        let sends = Arc::new(AtomicUsize::new(0));
        let root = tmp.path().to_owned();
        let observed = sends.clone();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            supervise(
                async {
                    let _ = stop_rx.await;
                    Ok(())
                },
                watch_with_clock(
                    &root,
                    45,
                    || base + chrono::Duration::from_std(start.elapsed()).unwrap(),
                    || {
                        observed.fetch_add(1, Ordering::SeqCst);
                        // The endpoint may accept delivery before its response hangs.
                        std::future::pending::<Result<()>>()
                    },
                ),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(46 * 60)).await;
        tokio::task::yield_now().await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60 * 60)).await;
        tokio::task::yield_now().await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
        stop_tx.send(()).unwrap();
        task.await.unwrap().unwrap();
        // A real tick allows a later incident, but the old watcher must be gone.
        store::record_completed_tick(tmp.path(), base).unwrap();
        tokio::time::advance(Duration::from_secs(60 * 60)).await;
        tokio::task::yield_now().await;
        assert_eq!(sends.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn abort_drops_inflight_delivery_and_restart_keeps_claim() {
        struct PendingDelivery(Arc<AtomicUsize>);
        impl Drop for PendingDelivery {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        store::start_deadman(tmp.path(), now - chrono::Duration::hours(1)).unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let observed = drops.clone();
        let root = tmp.path().to_owned();
        let task = tokio::spawn(async move {
            supervise(
                std::future::pending(),
                watch_with_clock(
                    &root,
                    45,
                    || now,
                    || {
                        let pending = PendingDelivery(observed.clone());
                        async move {
                            let _pending = pending;
                            std::future::pending::<Result<()>>().await
                        }
                    },
                ),
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        store::start_deadman(tmp.path(), now).unwrap();
        assert_eq!(
            store::claim_deadman_alert(tmp.path(), now + chrono::Duration::hours(1), 45).unwrap(),
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn muted_watcher_never_calls_delivery_or_claims() {
        let tmp = tempfile::tempdir().unwrap();
        let now = Utc::now();
        store::start_deadman(tmp.path(), now - chrono::Duration::hours(1)).unwrap();
        let watcher = watch_with_clock(
            tmp.path(),
            0,
            || now,
            || async { panic!("muted watcher sent an alert") },
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2 * 60 * 60), watcher)
                .await
                .is_err()
        );
        assert!(
            store::claim_deadman_alert(tmp.path(), now, 45)
                .unwrap()
                .is_some()
        );
    }
}
