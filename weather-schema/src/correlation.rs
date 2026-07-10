use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
};

use uuid::Uuid;

/// Generates opaque correlation IDs from a random process nonce and a
/// monotonically increasing sequence.
///
/// The nonce separates process lifetimes, while the sequence guarantees that
/// concurrent callers in one process never receive the same ID. Wall-clock
/// time and process IDs are deliberately not part of the uniqueness contract.
#[derive(Debug)]
pub struct CorrelationIdGenerator {
    nonce: Uuid,
    next_sequence: AtomicU64,
}

impl CorrelationIdGenerator {
    pub fn new() -> Self {
        Self {
            nonce: Uuid::new_v4(),
            next_sequence: AtomicU64::new(0),
        }
    }

    /// Returns the next opaque ID for `scope`.
    ///
    /// Sequence exhaustion fails instead of wrapping and reusing an ID.
    pub fn next(&self, scope: &str) -> String {
        let sequence = self
            .next_sequence
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .expect("correlation ID sequence exhausted");
        format!("{scope}-{}-{sequence:016x}", self.nonce)
    }

    #[cfg(test)]
    fn with_nonce(nonce: Uuid) -> Self {
        Self {
            nonce,
            next_sequence: AtomicU64::new(0),
        }
    }
}

impl Default for CorrelationIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a process-unique, concurrency-safe opaque correlation ID.
pub fn correlation_id(scope: &str) -> String {
    static GENERATOR: OnceLock<CorrelationIdGenerator> = OnceLock::new();
    GENERATOR
        .get_or_init(CorrelationIdGenerator::new)
        .next(scope)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, thread};

    use super::*;

    const FIXED_NONCE: u128 = 0x1234_5678_90ab_cdef_1234_5678_90ab_cdef;

    #[test]
    fn fixed_timestamp_requests_remain_unique() {
        let generator = CorrelationIdGenerator::with_nonce(Uuid::from_u128(FIXED_NONCE));
        let fixed_timestamp_unix_ms = 1_788_000_000_000_i64;
        let requests = (0..10_000)
            .map(|_| (fixed_timestamp_unix_ms, generator.next("request")))
            .collect::<HashSet<_>>();

        assert_eq!(requests.len(), 10_000);
        assert!(
            requests
                .iter()
                .all(|(timestamp, _)| *timestamp == fixed_timestamp_unix_ms)
        );
    }

    #[test]
    fn concurrent_generation_is_unique_at_high_volume() {
        const THREADS: usize = 8;
        const IDS_PER_THREAD: usize = 10_000;

        let workers = (0..THREADS)
            .map(|_| {
                thread::spawn(move || {
                    (0..IDS_PER_THREAD)
                        .map(|_| correlation_id("concurrent-event"))
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();

        let ids = workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("ID worker panicked"))
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), THREADS * IDS_PER_THREAD);
    }

    #[test]
    fn ids_are_opaque_and_scoped() {
        let generator = CorrelationIdGenerator::with_nonce(Uuid::from_u128(FIXED_NONCE));

        let request = generator.next("request");
        let event = generator.next("event");

        assert_eq!(
            request,
            "request-12345678-90ab-cdef-1234-567890abcdef-0000000000000000"
        );
        assert!(event.starts_with("event-12345678-90ab-cdef-1234-567890abcdef-"));
        assert_ne!(request, event);
    }

    #[test]
    #[should_panic(expected = "correlation ID sequence exhausted")]
    fn sequence_exhaustion_never_wraps() {
        let generator = CorrelationIdGenerator {
            nonce: Uuid::from_u128(FIXED_NONCE),
            next_sequence: AtomicU64::new(u64::MAX),
        };

        let _ = generator.next("request");
    }
}
