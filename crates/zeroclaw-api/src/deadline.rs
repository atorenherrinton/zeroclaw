//! A monotonic parent deadline, not a fresh duration on each retry/delegate.
//! Dropping an expired future is cancellation, never non-delivery evidence.
use std::{future::Future, time::Duration};
use tokio::time::Instant;

tokio::task_local! { pub static PARENT: Option<Instant>; }
pub fn current() -> Option<Instant> {
    PARENT.try_with(|d| *d).ok().flatten()
}
pub fn bounded_by_parent(local: Duration) -> Instant {
    let local = Instant::now() + local;
    current().map_or(local, |parent| parent.min(local))
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Turn,
    Provider,
    Tool,
    Delegate,
    Delivery,
}
#[derive(Debug)]
pub struct DeadlineExceeded {
    pub phase: Phase,
    /// False only when the expired parent prevented polling the child.
    pub started: bool,
}
impl std::fmt::Display for DeadlineExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "parent deadline exceeded during {:?}; started={}; reconcile in-flight effects before retry",
            self.phase, self.started
        )
    }
}
impl std::error::Error for DeadlineExceeded {}

pub async fn run_inherited<T>(
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    run_inherited_phase(Phase::Turn, future).await
}

pub async fn run_inherited_phase<T>(
    phase: Phase,
    future: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    if let Some(deadline) = current() {
        // An already expired parent must never poll a new operation.
        if deadline <= Instant::now() {
            return Err(DeadlineExceeded {
                phase,
                started: false,
            }
            .into());
        }
        tokio::time::timeout_at(deadline, future)
            .await
            .map_err(|_| {
                anyhow::Error::new(DeadlineExceeded {
                    phase,
                    started: true,
                })
            })?
    } else {
        future.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn phase_and_cancellation_survive_nested_deadlines() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        for phase in [Phase::Provider, Phase::Tool, Phase::Delegate] {
            let dropped = Arc::new(AtomicBool::new(false));
            let flag = dropped.clone();
            let error = PARENT
                .scope(
                    Some(Instant::now() + Duration::from_millis(10)),
                    run_inherited(run_inherited_phase(phase, async move {
                        let _guard = DropFlag(flag);
                        std::future::pending::<()>().await;
                        Ok(())
                    })),
                )
                .await
                .unwrap_err();
            let timeout = error.downcast_ref::<DeadlineExceeded>().unwrap();
            assert_eq!(timeout.phase, phase);
            assert!(timeout.started);
            assert!(dropped.load(Ordering::SeqCst));
        }
        let error = PARENT
            .scope(
                Some(Instant::now()),
                run_inherited_phase(Phase::Tool, async { Ok(()) }),
            )
            .await
            .unwrap_err();
        assert!(!error.downcast_ref::<DeadlineExceeded>().unwrap().started);
    }

    #[tokio::test]
    async fn nested_and_retry_work_cannot_extend_parent() {
        let deadline = Instant::now() + Duration::from_millis(20);
        PARENT
            .scope(Some(deadline), async {
                assert_eq!(bounded_by_parent(Duration::from_secs(600)), deadline);
                let error = run_inherited(async {
                    std::future::pending::<()>().await;
                    Ok(())
                })
                .await
                .unwrap_err();
                assert!(error.is::<DeadlineExceeded>());
                assert!(error.downcast_ref::<DeadlineExceeded>().unwrap().started);
                let polled = std::sync::atomic::AtomicBool::new(false);
                assert!(
                    run_inherited(async {
                        polled.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(())
                    })
                    .await
                    .is_err()
                );
                assert!(!polled.load(std::sync::atomic::Ordering::SeqCst));
            })
            .await;
        assert!(current().is_none());
    }
}
