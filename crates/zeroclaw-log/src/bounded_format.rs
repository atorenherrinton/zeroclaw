//! Bounds logger-owned formatting before tracing can allocate an arbitrary body.

use std::fmt::{self, Write};

pub(crate) const FIELD_BYTES: usize = 4 * 1024;
pub(crate) const TERMINAL_BYTES: usize = 16 * 1024;
pub(crate) const TRUNCATED: &str = " [log value truncated]";

/// The caller owns the destination. This owns only its remaining write budget;
/// it does not retain another copy of the event or change persistence policy.
pub(crate) struct BoundedWriter<'a> {
    inner: &'a mut dyn Write,
    remaining: usize,
    marker_bytes: usize,
    truncated: bool,
    failed: bool,
}

impl<'a> BoundedWriter<'a> {
    pub(crate) fn new(inner: &'a mut dyn Write, max_bytes: usize) -> Self {
        Self {
            inner,
            remaining: max_bytes.saturating_sub(TRUNCATED.len()),
            marker_bytes: max_bytes.min(TRUNCATED.len()),
            truncated: false,
            failed: false,
        }
    }

    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn finish(self, result: fmt::Result) -> fmt::Result {
        if self.failed {
            return Err(fmt::Error);
        }
        if self.truncated {
            self.inner.write_str(&TRUNCATED[..self.marker_bytes])
        } else {
            result
        }
    }
}

impl Write for BoundedWriter<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.failed || self.truncated {
            return Err(fmt::Error);
        }
        let mut end = text.len().min(self.remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if self.inner.write_str(&text[..end]).is_err() {
            self.failed = true;
            return Err(fmt::Error);
        }
        self.remaining -= end;
        if end < text.len() {
            self.truncated = true;
            return Err(fmt::Error);
        }
        Ok(())
    }
}

pub(crate) fn capture(
    max_bytes: usize,
    render: impl FnOnce(&mut BoundedWriter<'_>) -> fmt::Result,
) -> String {
    let mut text = String::new();
    let mut writer = BoundedWriter::new(&mut text, max_bytes);
    let result = render(&mut writer);
    if writer.finish(result).is_err() {
        // A user-supplied formatter may fail without exhausting the budget.
        // Its partial output must not be mistaken for a complete value.
        let failed = "[log value formatting failed]";
        return failed[..failed.len().min(max_bytes)].into();
    }
    text
}

pub(crate) fn text(value: &str, max_bytes: usize) -> String {
    capture(max_bytes, |out| out.write_str(value))
}

pub(crate) fn debug(value: &dyn fmt::Debug, max_bytes: usize) -> String {
    capture(max_bytes, |out| write!(out, "{value:?}"))
}

pub(crate) fn error(value: &(dyn std::error::Error + 'static)) -> String {
    capture(FIELD_BYTES, |out| {
        write!(out, "{value}")?;
        let mut current = value.source();
        // A cyclic or empty error chain must not loop forever.
        for _ in 0..32 {
            let Some(source) = current else { return Ok(()) };
            write!(out, ": {source}")?;
            current = source.source();
        }
        if current.is_some() {
            out.write_str(" [error sources truncated]")?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct LargeValue(Cell<usize>);
    impl fmt::Debug for LargeValue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for _ in 0..1_000_000 {
                self.0.set(self.0.get() + 1);
                f.write_str("🦀")?;
            }
            f.write_str("PRIVATE_TAIL")
        }
    }

    #[test]
    fn stops_formatting_at_utf8_budget_before_private_tail() {
        let value = LargeValue(Cell::new(0));
        let result = debug(&value, FIELD_BYTES);
        assert!(result.len() <= FIELD_BYTES);
        assert!(result.ends_with(TRUNCATED));
        assert!(!result.contains("PRIVATE_TAIL"));
        assert!(value.0.get() <= FIELD_BYTES / 4 + 1);
        assert_eq!(text("normal value", FIELD_BYTES), "normal value");
    }

    #[test]
    fn tiny_budgets_also_bound_markers_and_formatting_errors() {
        for budget in 0..64 {
            assert!(text(&"🦀".repeat(100), budget).len() <= budget);
            assert!(capture(budget, |_| Err(fmt::Error)).len() <= budget);
        }
    }

    #[test]
    fn formatter_and_destination_failures_remain_distinct_from_truncation() {
        struct Refuse;
        impl Write for Refuse {
            fn write_str(&mut self, _: &str) -> fmt::Result {
                Err(fmt::Error)
            }
        }
        let mut destination = Refuse;
        let mut writer = BoundedWriter::new(&mut destination, FIELD_BYTES);
        let result = writer.write_str("ordinary value");
        assert!(writer.finish(result).is_err());
        assert_eq!(
            capture(FIELD_BYTES, |_| Err(fmt::Error)),
            "[log value formatting failed]"
        );
    }

    #[test]
    fn cyclic_error_sources_terminate_even_when_display_is_empty() {
        #[derive(Debug)]
        struct Cycle;
        impl fmt::Display for Cycle {
            fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
                Ok(())
            }
        }
        impl std::error::Error for Cycle {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(self)
            }
        }
        let result = error(&Cycle);
        assert!(result.ends_with("[error sources truncated]"));
        assert!(result.len() < FIELD_BYTES);
    }
}
