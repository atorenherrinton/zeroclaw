//! Bounded measurement for shared encoded payloads.

/// A disposable formatting projection bounded by its complete JSON string size.
/// Each borrowed UTF-8 chunk is admitted before copying. A refused write drops
/// partial text and remains failed even if a formatter ignores the error.
/// This does not bound allocations or work performed inside custom formatters.
pub struct EncodedStringWriter {
    text: Option<String>,
    encoded_len: usize,
    limit: usize,
}

impl EncodedStringWriter {
    pub fn new(limit: usize) -> Self {
        Self {
            text: (limit >= 2).then(|| String::with_capacity(limit.saturating_sub(2).min(4094))),
            encoded_len: 2,
            limit,
        }
    }

    pub fn finish(self) -> Option<String> {
        self.text
    }
}

impl std::fmt::Write for EncodedStringWriter {
    fn write_str(&mut self, chunk: &str) -> std::fmt::Result {
        let Some(text) = &mut self.text else {
            return Err(std::fmt::Error);
        };
        // String contents compose across UTF-8 chunks; omit each chunk's quotes.
        let Some(size) = encoded_size(chunk, self.limit - self.encoded_len + 2) else {
            self.text = None;
            return Err(std::fmt::Error);
        };
        self.encoded_len += size - 2;
        text.push_str(chunk);
        Ok(())
    }
}

/// Count the complete JSON representation without allocating a serialized copy.
/// Stop serialization as soon as the limit is exceeded. Callers must treat
/// serialization failure as over budget, never as a zero-sized payload.
pub fn encoded_size<T: serde::Serialize + ?Sized>(value: &T, limit: usize) -> Option<usize> {
    struct Counter {
        used: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.used) {
                return Err(std::io::Error::other("encoded payload budget exceeded"));
            }
            self.used += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter { used: 0, limit };
    serde_json::to_writer(&mut counter, value).ok()?;
    Some(counter.used)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    #[test]
    fn string_writer_matches_encoding_at_every_boundary() {
        let chunks = ["hello", "😀", "\0\n", "\"\\", "tail"];
        let expected = chunks.concat();
        let size = serde_json::to_vec(&expected).unwrap().len();
        for limit in 0..=size + 1 {
            let mut writer = EncodedStringWriter::new(limit);
            for chunk in chunks {
                let _ = writer.write_str(chunk);
            }
            assert_eq!(writer.finish(), (limit >= size).then(|| expected.clone()));
        }
    }

    #[test]
    fn string_writer_rejection_is_sticky_and_empty_string_needs_quotes() {
        assert!(EncodedStringWriter::new(1).finish().is_none());
        assert_eq!(EncodedStringWriter::new(2).finish().as_deref(), Some(""));
        let mut writer = EncodedStringWriter::new(8);
        writer.write_str("prefix").unwrap();
        assert!(writer.write_str("\0").is_err());
        assert!(writer.write_str("").is_err());
        assert!(writer.finish().is_none());
    }
}
