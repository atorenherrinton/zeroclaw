//! Bounded measurement for shared encoded payloads.

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
