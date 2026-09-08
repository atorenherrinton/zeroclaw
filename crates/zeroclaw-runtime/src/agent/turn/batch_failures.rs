//! Transient ownership of every terminal tool error in a dispatched batch.

use super::is_tool_loop_cancelled;
use crate::agent::tool_execution::bounded_observer_text;

/// The primary error stays in anyhow's cause chain for existing typed routing.
/// Other original errors live here, keyed by position in the original model
/// batch (including preparation-only calls). Do not format their payloads.
/// History or ResultBudgetExceeded owns the corresponding call metadata.
pub(crate) struct RetainedToolFailures {
    pub(crate) primary_call_index: usize,
    pub(crate) siblings: Vec<(usize, anyhow::Error)>,
    summary: String,
}

impl std::fmt::Debug for RetainedToolFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedToolFailures")
            .field("primary_call_index", &self.primary_call_index)
            .field("sibling_count", &self.siblings.len())
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RetainedToolFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary)
    }
}

struct PrimaryFailure {
    call_index: usize,
    error: anyhow::Error,
    // A bounded display projection, not a replacement for the original error.
    summary: String,
}

/// Moves errors out of dispatch slots without reducing the batch to one owner.
#[derive(Default)]
pub(crate) struct TerminalFailures {
    primary: Option<PrimaryFailure>,
    siblings: Vec<(usize, anyhow::Error)>,
}

impl TerminalFailures {
    pub(crate) fn push(&mut self, call_index: usize, error: anyhow::Error, summary: &str) {
        // Preserve existing precedence: delivery wins; otherwise the first
        // non-cancellation wins. Equal-priority failures retain dispatch order.
        let replace = self.primary.as_ref().is_none_or(|previous| {
            (is_tool_loop_cancelled(&previous.error) && !is_tool_loop_cancelled(&error))
                || (error.is::<zeroclaw_api::delivery::DeliveryFailure>()
                    && !previous
                        .error
                        .is::<zeroclaw_api::delivery::DeliveryFailure>())
        });
        if !replace {
            self.siblings.push((call_index, error));
            return;
        }
        let primary = PrimaryFailure {
            call_index,
            error,
            summary: bounded_observer_text(summary),
        };
        if let Some(previous) = self.primary.replace(primary) {
            self.siblings.push((previous.call_index, previous.error));
        }
    }

    pub(crate) fn into_error(mut self) -> Option<anyhow::Error> {
        let primary = self.primary?;
        if self.siblings.is_empty() {
            return Some(primary.error);
        }
        self.siblings.sort_by_key(|(index, _)| *index);
        Some(primary.error.context(RetainedToolFailures {
            primary_call_index: primary.call_index,
            siblings: self.siblings,
            summary: primary.summary,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_batch_retains_original_positions_and_bounds_context_summary() {
        let mut failures = TerminalFailures::default();
        let max_calls = zeroclaw_tools::output_budget::MAX_BATCH_CALLS;
        for call_index in 0..max_calls - 1 {
            failures.push(
                call_index,
                zeroclaw_api::deadline::DeadlineExceeded {
                    phase: zeroclaw_api::deadline::Phase::Tool,
                    started: true,
                }
                .into(),
                "fixture deadline",
            );
        }
        failures.push(
            max_calls - 1,
            zeroclaw_api::delivery::DeliveryFailure {
                outcome: zeroclaw_api::delivery::EffectOutcome::PossiblyApplied,
                chunk_index: 1,
                total_chunks: 2,
                confirmed_chunks: 1,
            }
            .into(),
            &format!("{}token=fixture-private-value", "😀".repeat(1100)),
        );
        let error = failures.into_error().unwrap();
        assert!(error.is::<zeroclaw_api::delivery::DeliveryFailure>());
        let evidence = error.downcast_ref::<RetainedToolFailures>().unwrap();
        assert_eq!(evidence.primary_call_index, max_calls - 1);
        assert_eq!(evidence.siblings.len(), max_calls - 1);
        for (position, (index, original)) in evidence.siblings.iter().enumerate() {
            assert_eq!(*index, position);
            assert!(
                original
                    .downcast_ref::<zeroclaw_api::deadline::DeadlineExceeded>()
                    .unwrap()
                    .started
            );
        }
        let summary = error.to_string();
        assert!(summary.len() <= 4096);
        assert!(summary.contains("omitted"));
        assert!(!summary.contains("fixture-private-value"));
        assert!(format!("{evidence:?}").len() < 200);
    }

    #[test]
    fn empty_and_single_failure_keep_existing_routing_and_display() {
        assert!(TerminalFailures::default().into_error().is_none());
        let mut failures = TerminalFailures::default();
        let original = anyhow::Error::new(super::super::ToolLoopCancelled);
        let message = original.to_string();
        failures.push(3, original, "fixture display projection");
        let error = failures.into_error().unwrap();
        assert!(is_tool_loop_cancelled(&error));
        assert_eq!(error.to_string(), message);
        assert!(error.downcast_ref::<RetainedToolFailures>().is_none());
    }
}
