// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

/// Error module — mirrors the crate's `error` module so that the
/// `include!`-ed concurrency source can resolve `crate::error::*`.
#[allow(dead_code)]
pub mod error {
    include!("../src/error.rs");
}

/// Pull in private modules so tests can access internal types.
/// The `mod ring_buffer;` inside concurrency.rs resolves to
/// `tests/concurrency/ring_buffer.rs`, which includes the source and
/// adds test helpers.
#[allow(dead_code)]
mod concurrency {
    include!("../src/concurrency.rs");

    impl<T> ConcurrentRing<T> {
        /// Test-only: acquire the mutex and panic while holding it, poisoning
        /// the lock. Must be called from a thread where the panic can be
        /// caught (e.g. via `std::thread::spawn` + `join`).
        pub fn poison_mutex(&self) {
            let _guard = self.ring.lock().unwrap();
            panic!("intentional panic to poison mutex");
        }
    }
}

use concurrency::{ChannelMode, ConcurrentRing, DrainMode, ReportChannel};
use error::{CaducusError, CaducusErrorKind};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const DEFAULT_TTL: Duration = Duration::from_secs(5);
const SHORT_TTL: Duration = Duration::from_millis(50);
const MIN_TTL: Duration = Duration::from_millis(1);
const MAX_TTL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// A simple ReportChannel implementation for testing.
struct TestChannel;

impl<T: Send + 'static> ReportChannel<T> for TestChannel {
    fn send(&self, _item: T) -> Result<(), T> {
        Ok(())
    }
}

fn make_ring(capacity: usize, ttl: Duration) -> ConcurrentRing<i32> {
    ConcurrentRing::new(capacity, ttl, ChannelMode::Mpsc, None, None).expect("valid TTL")
}

fn make_spsc_ring(
    capacity: usize,
    ttl: Duration,
    expiry: Option<Arc<dyn ReportChannel<i32>>>,
    shutdown: Option<Arc<dyn ReportChannel<i32>>>,
) -> ConcurrentRing<i32> {
    ConcurrentRing::new(capacity, ttl, ChannelMode::Spsc, expiry, shutdown).expect("valid TTL")
}

fn send_val(ring: &ConcurrentRing<i32>, val: i32) {
    ring.send_mpsc(val, None, None)
        .expect("send should succeed");
}

fn send_val_spsc(ring: &ConcurrentRing<i32>, val: i32) {
    ring.send_spsc(val).expect("send should succeed");
}

// ---------------------------------------------------------------------------
// Basic send and drain-and-claim
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_then_drain_and_claim_returns_item() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 42);
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert!(result.expired.is_empty());
    let live = result.live.expect("should claim live item");
    assert_eq!(live.item, 42);
}

#[tokio::test]
async fn fifo_ordering_through_concurrency_layer() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    send_val(&inner, 2);
    send_val(&inner, 3);
    let a = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let b = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let c = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(a.live.unwrap().item, 1);
    assert_eq!(b.live.unwrap().item, 2);
    assert_eq!(c.live.unwrap().item, 3);
}

// ---------------------------------------------------------------------------
// Send errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_full_returns_item() {
    let inner = Arc::new(make_ring(2, DEFAULT_TTL));
    send_val(&inner, 1);
    send_val(&inner, 2);
    let err = inner.send_mpsc(3, None, None).unwrap_err();
    match err.kind {
        CaducusErrorKind::Full(v) => assert_eq!(v, 3),
        other => panic!("expected Full, got {other:?}"),
    }
}

#[tokio::test]
async fn send_after_shutdown_returns_item() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    inner.shutdown();
    let err = inner.send_mpsc(1, None, None).unwrap_err();
    match err.kind {
        CaducusErrorKind::Shutdown(v) => assert_eq!(v, 1),
        other => panic!("expected Shutdown, got {other:?}"),
    }
}

#[tokio::test]
async fn send_error_into_inner() {
    let inner = Arc::new(make_ring(1, DEFAULT_TTL));
    send_val(&inner, 1);
    let err = inner.send_mpsc(99, None, None).unwrap_err();
    assert_eq!(err.into_inner(), Some(99));
}

#[tokio::test]
async fn send_shutdown_priority_over_full() {
    // When buffer is both full and shut down, Shutdown takes priority.
    let inner = Arc::new(make_ring(1, DEFAULT_TTL));
    send_val(&inner, 1);
    assert!(inner.send_mpsc(2, None, None).is_err()); // Full.
    inner.shutdown();
    let err = inner.send_mpsc(3, None, None).unwrap_err();
    match err.kind {
        CaducusErrorKind::Shutdown(v) => assert_eq!(v, 3),
        other => panic!("expected Shutdown, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Drain returns None on empty buffer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_and_claim_empty_returns_none() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert!(result.live.is_none());
    assert!(result.expired.is_empty());
    assert!(!result.is_shutdown);
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shutdown_returns_all_items() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    send_val(&inner, 2);
    send_val(&inner, 3);
    let items = inner.shutdown();
    let vals: Vec<i32> = items.into_iter().map(|r| r.item).collect();
    assert_eq!(vals, vec![1, 2, 3]);
}

#[tokio::test]
async fn shutdown_on_empty_buffer() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let items = inner.shutdown();
    assert!(items.is_empty());
    assert!(inner.is_shutdown());
}

#[tokio::test]
async fn is_shutdown_false_initially() {
    let inner = make_ring(4, DEFAULT_TTL);
    assert!(!inner.is_shutdown());
}

#[tokio::test]
async fn is_shutdown_true_after_shutdown() {
    let inner = make_ring(4, DEFAULT_TTL);
    inner.shutdown();
    assert!(inner.is_shutdown());
}

#[tokio::test]
async fn drain_and_claim_after_shutdown_reports_shutdown() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    inner.shutdown();
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert!(result.live.is_none());
    assert!(result.is_shutdown);
}

#[tokio::test]
async fn double_shutdown_returns_empty() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    let first = inner.shutdown();
    assert_eq!(first.len(), 1);
    let second = inner.shutdown();
    assert!(second.is_empty());
}

// ---------------------------------------------------------------------------
// Expired-head drain: DrainAndClaim skips expired, returns live
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_and_claim_skips_expired_returns_live() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1); // Will expire quickly.

    // Wait for the item to expire.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Push a fresh item with a long TTL.
    inner.update_ttl(DEFAULT_TTL).unwrap();
    send_val(&inner, 2);

    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    // Item 1 was expired and drained. Item 2 is live.
    assert_eq!(result.expired.len(), 1);
    assert_eq!(result.expired[0].item, 1);
    let live = result.live.expect("should claim live item");
    assert_eq!(live.item, 2);
}

#[tokio::test]
async fn drain_and_claim_returns_none_when_all_expired() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1);

    // Wait for the item to expire.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(result.expired.len(), 1);
    assert!(result.live.is_none());
    assert!(!result.is_shutdown);
}

// ---------------------------------------------------------------------------
// Drain path (DrainOnly)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_only_returns_expired_head() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1);

    // Wait for expiry.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert_eq!(result.expired.len(), 1);
    assert_eq!(result.expired[0].item, 1);
    assert!(result.live.is_none()); // DrainOnly never claims live
}

#[tokio::test]
async fn drain_only_drains_multiple_expired_heads() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1);
    send_val(&inner, 2);
    send_val(&inner, 3);

    // Wait for all to expire.
    tokio::time::sleep(Duration::from_millis(80)).await;

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert_eq!(result.expired.len(), 3);
    assert_eq!(result.expired[0].item, 1);
    assert_eq!(result.expired[1].item, 2);
    assert_eq!(result.expired[2].item, 3);
    assert!(result.next_deadline.is_none()); // buffer empty after drain
    assert!(result.live.is_none());
}

#[tokio::test]
async fn drain_only_stops_at_live_head() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1); // will expire

    // Update TTL so items 2 and 3 won't expire.
    inner.update_ttl(DEFAULT_TTL).unwrap();
    send_val(&inner, 2);
    send_val(&inner, 3);

    tokio::time::sleep(Duration::from_millis(80)).await;

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert_eq!(result.expired.len(), 1);
    assert_eq!(result.expired[0].item, 1);
    assert!(result.next_deadline.is_some()); // live head remains
    assert!(result.live.is_none());
}

#[tokio::test]
async fn drain_only_returns_empty_when_head_is_live() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert!(result.expired.is_empty());
    assert!(result.next_deadline.is_some());
    assert!(result.live.is_none());
}

#[tokio::test]
async fn drain_only_returns_empty_when_buffer_empty() {
    let inner = make_ring(4, DEFAULT_TTL);
    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert!(result.expired.is_empty());
    assert!(result.next_deadline.is_none());
    assert!(result.live.is_none());
}

#[tokio::test]
async fn drain_returns_next_deadline() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let before = Instant::now();
    send_val(&inner, 1);
    let after = Instant::now();

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    let deadline = result.next_deadline.expect("should have deadline");
    // Deadline should be between before+ttl and after+ttl.
    assert!(deadline >= before + DEFAULT_TTL);
    assert!(deadline <= after + DEFAULT_TTL);
}

#[tokio::test]
async fn drain_returns_shutdown_flag() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert!(!result.is_shutdown);

    inner.shutdown();

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert!(result.is_shutdown);
}

#[tokio::test]
async fn drain_and_claim_after_drain_only_removes_expired() {
    // After DrainOnly removes the expired head, a subsequent DrainAndClaim
    // should immediately return the live item behind it.
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    send_val(&inner, 1); // Will expire.

    // Update TTL so item 2 won't expire.
    inner.update_ttl(DEFAULT_TTL).unwrap();
    send_val(&inner, 2);

    // Wait for item 1 to expire.
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Drain expired only.
    let reclaimed = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert_eq!(reclaimed.expired.len(), 1);
    assert_eq!(reclaimed.expired[0].item, 1);

    // Now claim the live item.
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let live = result.live.unwrap();
    assert_eq!(live.item, 2);
}

// ---------------------------------------------------------------------------
// TTL update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_ttl_changes_future_sends() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let new_ttl = Duration::from_secs(10);
    inner.update_ttl(new_ttl).unwrap();

    let before = Instant::now();
    send_val(&inner, 1);
    let after = Instant::now();

    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let live = result.live.unwrap();
    // Expiry should reflect the new TTL.
    assert!(live.expires_at >= before + new_ttl);
    assert!(live.expires_at <= after + new_ttl);
}

#[tokio::test]
async fn update_ttl_below_minimum_rejected() {
    let inner = make_ring(4, DEFAULT_TTL);
    let err = inner.update_ttl(Duration::from_micros(999)).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn update_ttl_above_maximum_rejected() {
    let inner = make_ring(4, DEFAULT_TTL);
    let err = inner
        .update_ttl(MAX_TTL + Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn update_ttl_accepts_minimum_ttl() {
    let inner = make_ring(4, DEFAULT_TTL);
    assert!(inner.update_ttl(MIN_TTL).is_ok());
}

#[tokio::test]
async fn update_ttl_accepts_maximum_ttl() {
    let inner = make_ring(4, DEFAULT_TTL);
    assert!(inner.update_ttl(MAX_TTL).is_ok());
}

#[tokio::test]
async fn new_rejects_ttl_below_minimum() {
    let result =
        ConcurrentRing::<i32>::new(4, Duration::from_micros(999), ChannelMode::Mpsc, None, None);
    let err = result.err().expect("should reject ttl below 1ms");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn new_rejects_ttl_above_maximum() {
    let result = ConcurrentRing::<i32>::new(
        4,
        MAX_TTL + Duration::from_secs(1),
        ChannelMode::Mpsc,
        None,
        None,
    );
    let err = result.err().expect("should reject ttl above 1 year");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn new_accepts_minimum_ttl() {
    assert!(ConcurrentRing::<i32>::new(4, MIN_TTL, ChannelMode::Mpsc, None, None).is_ok());
}

#[tokio::test]
async fn new_accepts_maximum_ttl() {
    assert!(ConcurrentRing::<i32>::new(4, MAX_TTL, ChannelMode::Mpsc, None, None).is_ok());
}

#[tokio::test]
async fn update_ttl_does_not_affect_existing_items() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    // Change TTL to something very different.
    inner.update_ttl(Duration::from_secs(60)).unwrap();
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let live = result.live.unwrap();
    // Item's expiry should reflect the original TTL, not the new one.
    // It was pushed with DEFAULT_TTL (5s), so it should expire around now + 5s.
    let expected_min = Instant::now() + Duration::from_secs(3);
    let expected_max = Instant::now() + Duration::from_secs(6);
    assert!(live.expires_at >= expected_min);
    assert!(live.expires_at <= expected_max);
}

// ---------------------------------------------------------------------------
// Capacity update
// ---------------------------------------------------------------------------

#[tokio::test]
async fn update_capacity_growth() {
    let inner = Arc::new(make_ring(2, DEFAULT_TTL));
    send_val(&inner, 1);
    send_val(&inner, 2);
    // Full at capacity 2.
    assert!(inner.send_mpsc(3, None, None).is_err());
    // Grow.
    inner.update_capacity(4);
    send_val(&inner, 3);
    send_val(&inner, 4);
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(result.live.unwrap().item, 1);
}

#[tokio::test]
async fn update_capacity_shrink() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    send_val(&inner, 1);
    inner.update_capacity(2);
    // Should still have the item.
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(result.live.unwrap().item, 1);
}

// ---------------------------------------------------------------------------
// MPSC report channel handles travel through concurrency layer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn send_mpsc_with_channels_returns_channels_on_drain() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let expiry_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let shutdown_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    inner
        .send_mpsc(1, Some(expiry_ch), Some(shutdown_ch))
        .expect("send should succeed");

    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let live = result.live.unwrap();
    assert_eq!(live.item, 1);
    assert!(live.expiry_channel.is_some());
    assert!(live.shutdown_channel.is_some());
}

#[tokio::test]
async fn shutdown_returns_items_with_channels() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    inner
        .send_mpsc(1, Some(ch.clone()), Some(ch.clone()))
        .expect("send should succeed");
    let items = inner.shutdown();
    assert_eq!(items.len(), 1);
    assert!(items[0].expiry_channel.is_some());
    assert!(items[0].shutdown_channel.is_some());
}

#[tokio::test]
async fn drain_only_returns_item_with_channels() {
    let inner = Arc::new(make_ring(4, SHORT_TTL));
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    inner
        .send_mpsc(1, Some(ch.clone()), Some(ch.clone()))
        .expect("send should succeed");

    tokio::time::sleep(Duration::from_millis(80)).await;

    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert_eq!(result.expired.len(), 1);
    assert_eq!(result.expired[0].item, 1);
    assert!(result.expired[0].expiry_channel.is_some());
    assert!(result.expired[0].shutdown_channel.is_some());
}

// ---------------------------------------------------------------------------
// Poisoned mutex recovery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn poisoned_mutex_recovery() {
    // Poison the mutex by panicking while holding the lock on a separate
    // thread. After poisoning, all operations should still work because
    // lock() recovers via PoisonError::into_inner.
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));

    // Send an item before poisoning so we can verify it survives.
    send_val(&inner, 1);

    // Poison the mutex from another thread.
    let inner_clone = Arc::clone(&inner);
    let handle = std::thread::spawn(move || {
        inner_clone.poison_mutex();
    });
    // The thread panicked — join returns Err.
    assert!(handle.join().is_err());

    // The mutex is now poisoned. Verify all operation families recover.
    // Send: should succeed on the poisoned mutex.
    send_val(&inner, 2);

    // Drain-and-claim: should return items in FIFO order.
    let r1 = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(r1.live.unwrap().item, 1);
    let r2 = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert_eq!(r2.live.unwrap().item, 2);

    // Drain-only: should work on poisoned mutex.
    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    assert!(result.expired.is_empty());
    assert!(result.next_deadline.is_none());

    // Config: should work on poisoned mutex.
    inner.update_ttl(Duration::from_secs(10)).unwrap();
    inner.update_capacity(8);

    // Shutdown: should work on poisoned mutex.
    send_val(&inner, 3);
    let drained = inner.shutdown();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].item, 3);
    assert!(inner.is_shutdown());
}

// ---------------------------------------------------------------------------
// Notify handles
// ---------------------------------------------------------------------------

#[tokio::test]
async fn notify_reclaimer_handle_returns_shared_handle() {
    let inner = make_ring(4, DEFAULT_TTL);
    let h1 = inner.notify_reclaimer_handle();
    let h2 = inner.notify_reclaimer_handle();
    assert!(Arc::ptr_eq(&h1, &h2));
}

#[tokio::test]
async fn notify_receiver_handle_returns_shared_handle() {
    let inner = make_ring(4, DEFAULT_TTL);
    let h1 = inner.notify_receiver_handle();
    let h2 = inner.notify_receiver_handle();
    assert!(Arc::ptr_eq(&h1, &h2));
}

#[tokio::test]
async fn notify_handles_are_distinct() {
    let inner = make_ring(4, DEFAULT_TTL);
    let reclaimer = inner.notify_reclaimer_handle();
    let receiver = inner.notify_receiver_handle();
    assert!(!Arc::ptr_eq(&reclaimer, &receiver));
}

// ---------------------------------------------------------------------------
// CaducusError display
// ---------------------------------------------------------------------------

#[tokio::test]
async fn caducus_error_display() {
    let err = inner_update_ttl_below_minimum_error();
    assert_eq!(format!("{err}"), "invalid argument");
}

fn inner_update_ttl_below_minimum_error() -> CaducusError {
    let inner = make_ring(4, DEFAULT_TTL);
    inner.update_ttl(Duration::from_micros(999)).unwrap_err()
}

// ---------------------------------------------------------------------------
// Edge: drain after shutdown reports shutdown flag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_and_claim_shutdown_empty_reports_shutdown() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    inner.shutdown();
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    assert!(result.live.is_none());
    assert!(result.is_shutdown);
}

// ---------------------------------------------------------------------------
// SPSC send through concurrency layer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spsc_send_then_drain_and_claim() {
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let inner = Arc::new(make_spsc_ring(4, DEFAULT_TTL, Some(ch.clone()), Some(ch)));
    send_val_spsc(&inner, 42);
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let live = result.live.expect("should claim live item");
    assert_eq!(live.item, 42);
    // Ring-level channels populate the PopResult.
    assert!(live.expiry_channel.is_some());
    assert!(live.shutdown_channel.is_some());
}

#[tokio::test]
async fn spsc_send_returns_invalid_pattern_in_mpsc_mode() {
    let inner = Arc::new(make_ring(4, DEFAULT_TTL));
    let err = inner.send_spsc(1).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidPattern(1));
}

#[tokio::test]
async fn mpsc_send_returns_invalid_pattern_in_spsc_mode() {
    let inner = Arc::new(make_spsc_ring(4, DEFAULT_TTL, None, None));
    let err = inner.send_mpsc(1, None, None).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidPattern(1));
}

// ---------------------------------------------------------------------------
// TTL-shrink: full-scan drain via concurrency layer
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_finds_non_head_expired_after_ttl_shrink() {
    let inner = Arc::new(make_ring(4, Duration::from_secs(60)));
    send_val(&inner, 1); // long deadline
    inner
        .update_ttl(Duration::from_millis(20))
        .expect("update_ttl ok");
    send_val(&inner, 2); // short deadline
    tokio::time::sleep(Duration::from_millis(60)).await;
    let result = inner.drain(Instant::now(), DrainMode::DrainOnly);
    let expired_items: Vec<i32> = result.expired.into_iter().map(|p| p.item).collect();
    assert_eq!(
        expired_items,
        vec![2],
        "non-head expired item must be reported"
    );
    // Surviving head deadline (item 1, ~60s out) should still be the next
    // deadline.
    let next = result
        .next_deadline
        .expect("next_deadline should be present");
    let now = Instant::now();
    assert!(
        next > now + Duration::from_secs(50),
        "surviving head should keep its long deadline"
    );
}

#[tokio::test]
async fn drain_and_claim_after_partial_expiry() {
    let inner = Arc::new(make_ring(4, Duration::from_secs(60)));
    send_val(&inner, 1); // A: long
    inner.update_ttl(Duration::from_millis(20)).unwrap();
    send_val(&inner, 2); // B: short
    inner.update_ttl(Duration::from_secs(60)).unwrap();
    send_val(&inner, 3); // C: long
    tokio::time::sleep(Duration::from_millis(60)).await;
    let result = inner.drain(Instant::now(), DrainMode::DrainAndClaim);
    let expired: Vec<i32> = result.expired.into_iter().map(|p| p.item).collect();
    assert_eq!(expired, vec![2]);
    // After expiry, the next claim should yield A (the FIFO head among
    // survivors).
    let live = result.live.expect("should claim live item");
    assert_eq!(live.item, 1);
}
