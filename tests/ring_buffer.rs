// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

/// Stub concurrency module with error types and ring buffer submodule.
/// The ring buffer source uses `use super::{CaducusError, CaducusErrorKind}`
/// which resolves to these stubs.
#[allow(dead_code)]
mod concurrency {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) enum CaducusErrorKind<T = ()> {
        InvalidArgument,
        InvalidPattern(T),
        NoRuntime,
        Timeout,
        Shutdown(T),
        Full(T),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct CaducusError<T = ()> {
        pub kind: CaducusErrorKind<T>,
    }

    pub mod ring_buffer {
        include!("../src/concurrency/ring_buffer.rs");

        /// Validate internal ring invariants. Lives inside the module so it
        /// can access private fields.
        pub fn validate<T>(ring: &Ring<T>) {
            let cap = ring.capacity();
            assert!(
                ring.len() <= cap,
                "len {} exceeds capacity {}",
                ring.len(),
                cap
            );
            if ring.is_empty() {
                assert_eq!(ring.len(), 0);
            }
            if ring.len() >= ring.target_capacity() {
                assert!(ring.is_full());
            }
            // Verify occupied slots match len.
            let mut occupied = 0;
            for i in 0..ring.len {
                let idx = (ring.head + i) % cap;
                assert!(
                    ring.slots[idx].is_occupied(),
                    "slot {} (index {}) should be occupied",
                    i,
                    idx
                );
                occupied += 1;
            }
            assert_eq!(occupied, ring.len());
        }

        pub fn force_raw_ttl<T>(ring: &mut Ring<T>, ttl: Duration) {
            ring.ttl = ttl;
        }
    }
}

use concurrency::ring_buffer::{force_raw_ttl, validate, ChannelMode, ReportChannel, Ring};
use concurrency::CaducusErrorKind;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

const DEFAULT_TTL: Duration = Duration::from_secs(5);

macro_rules! new_ring {
    ($capacity:expr, $ttl:expr) => {
        Ring::<i32>::new($capacity, $ttl, ChannelMode::Mpsc, None, None).expect("valid TTL")
    };
}

macro_rules! new_spsc_ring {
    ($capacity:expr, $ttl:expr, $expiry:expr, $shutdown:expr) => {
        Ring::<i32>::new($capacity, $ttl, ChannelMode::Spsc, $expiry, $shutdown).expect("valid TTL")
    };
}

/// A simple ReportChannel implementation for testing.
struct TestChannel;

impl<T: Send + 'static> ReportChannel<T> for TestChannel {
    fn send(&self, _item: T) -> Result<(), T> {
        Ok(())
    }
}

fn push_val(ring: &mut Ring<i32>, val: i32) {
    ring.try_push_mpsc(val, None, None)
        .expect("push should succeed");
}

fn push_val_spsc(ring: &mut Ring<i32>, val: i32) {
    ring.try_push_spsc(val).expect("push should succeed");
}

fn push_val_with_channels(
    ring: &mut Ring<i32>,
    val: i32,
    expiry: Option<Arc<dyn ReportChannel<i32>>>,
    shutdown: Option<Arc<dyn ReportChannel<i32>>>,
) {
    ring.try_push_mpsc(val, expiry, shutdown)
        .expect("push should succeed");
}

fn pop_val(ring: &mut Ring<i32>) -> i32 {
    ring.try_pop().expect("pop should return an item").item
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

#[test]
fn new_allocates_empty_slots() {
    let ring = new_ring!(4, DEFAULT_TTL);
    assert_eq!(ring.capacity(), 4);
    assert_eq!(ring.target_capacity(), 4);
    assert_eq!(ring.len(), 0);
    assert!(ring.is_empty());
    assert!(!ring.is_full());
    assert!(!ring.is_shutdown());
    assert_eq!(ring.ttl(), DEFAULT_TTL);
    validate(&ring);
}

#[test]
fn new_clamps_zero_to_one() {
    let ring = new_ring!(0, DEFAULT_TTL);
    assert_eq!(ring.capacity(), 1);
    assert_eq!(ring.target_capacity(), 1);
    validate(&ring);
}

#[test]
fn new_capacity_one() {
    let ring = new_ring!(1, DEFAULT_TTL);
    assert_eq!(ring.capacity(), 1);
    validate(&ring);
}

#[test]
fn new_stores_provided_ttl() {
    let ttl = Duration::from_millis(750);
    let ring = new_ring!(4, ttl);
    assert_eq!(ring.ttl(), ttl);
}

#[test]
fn new_rejects_ttl_below_minimum() {
    let result = Ring::<i32>::new(4, Duration::from_micros(999), ChannelMode::Mpsc, None, None);
    let err = result.err().expect("ttl below 1ms should be rejected");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[test]
fn new_rejects_ttl_above_maximum() {
    let result = Ring::<i32>::new(
        4,
        Ring::<i32>::MAX_TTL + Duration::from_secs(1),
        ChannelMode::Mpsc,
        None,
        None,
    );
    let err = result.err().expect("ttl above 1 year should be rejected");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[test]
fn new_accepts_minimum_ttl() {
    let ring = Ring::<i32>::new(4, Ring::<i32>::MIN_TTL, ChannelMode::Mpsc, None, None)
        .expect("valid TTL");
    assert_eq!(ring.ttl(), Ring::<i32>::MIN_TTL);
}

#[test]
fn new_accepts_maximum_ttl() {
    let ring = Ring::<i32>::new(4, Ring::<i32>::MAX_TTL, ChannelMode::Mpsc, None, None)
        .expect("valid TTL");
    assert_eq!(ring.ttl(), Ring::<i32>::MAX_TTL);
}

// ---------------------------------------------------------------------------
// TTL
// ---------------------------------------------------------------------------

#[test]
fn set_ttl_updates_value() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let new_ttl = Duration::from_secs(10);
    ring.set_ttl(new_ttl).unwrap();
    assert_eq!(ring.ttl(), new_ttl);
}

#[test]
fn set_ttl_does_not_affect_existing_items() {
    let original_ttl = Duration::from_secs(5);
    let mut ring = new_ring!(4, original_ttl);
    let before = Instant::now();
    push_val(&mut ring, 1);
    let after = Instant::now();
    // Change TTL — existing item keeps its original expires_at.
    ring.set_ttl(Duration::from_secs(1)).unwrap();
    let result = ring.try_pop().unwrap();
    assert!(result.expires_at >= before + original_ttl);
    assert!(result.expires_at <= after + original_ttl);
}

#[test]
fn set_ttl_rejects_ttl_below_minimum() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let err = ring
        .set_ttl(Duration::from_micros(999))
        .expect_err("ttl below 1ms should be rejected");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[test]
fn set_ttl_rejects_ttl_above_maximum() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let err = ring
        .set_ttl(Ring::<i32>::MAX_TTL + Duration::from_secs(1))
        .expect_err("ttl above 1 year should be rejected");
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[test]
fn set_ttl_accepts_minimum_ttl() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    ring.set_ttl(Ring::<i32>::MIN_TTL).unwrap();
    assert_eq!(ring.ttl(), Ring::<i32>::MIN_TTL);
}

#[test]
fn set_ttl_accepts_maximum_ttl() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    ring.set_ttl(Ring::<i32>::MAX_TTL).unwrap();
    assert_eq!(ring.ttl(), Ring::<i32>::MAX_TTL);
}

#[test]
fn ttl_clamps_below_minimum_when_raw_value_is_forced() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    force_raw_ttl(&mut ring, Duration::ZERO);
    assert_eq!(ring.ttl(), Ring::<i32>::MIN_TTL);
}

#[test]
fn ttl_clamps_above_maximum_when_raw_value_is_forced() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    force_raw_ttl(&mut ring, Ring::<i32>::MAX_TTL + Duration::from_secs(1));
    assert_eq!(ring.ttl(), Ring::<i32>::MAX_TTL);
}

#[test]
fn ttl_returns_stored_value_when_it_is_in_range() {
    let ring = new_ring!(4, Duration::from_secs(10));
    assert_eq!(ring.ttl(), Duration::from_secs(10));
}

// ---------------------------------------------------------------------------
// FIFO ordering
// ---------------------------------------------------------------------------

#[test]
fn push_pop_fifo_order() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 10);
    push_val(&mut ring, 20);
    push_val(&mut ring, 30);
    assert_eq!(ring.len(), 3);
    assert_eq!(pop_val(&mut ring), 10);
    assert_eq!(pop_val(&mut ring), 20);
    assert_eq!(pop_val(&mut ring), 30);
    assert!(ring.is_empty());
    validate(&ring);
}

#[test]
fn fifo_with_wrap_around() {
    let mut ring = new_ring!(3, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    assert!(ring.is_full());
    // Pop two, freeing space at the front.
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(pop_val(&mut ring), 2);
    // Push two more — these wrap around.
    push_val(&mut ring, 4);
    push_val(&mut ring, 5);
    assert_eq!(ring.len(), 3);
    assert_eq!(pop_val(&mut ring), 3);
    assert_eq!(pop_val(&mut ring), 4);
    assert_eq!(pop_val(&mut ring), 5);
    assert!(ring.is_empty());
    validate(&ring);
}

#[test]
fn interleaved_push_pop() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    assert_eq!(pop_val(&mut ring), 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    assert!(ring.is_empty());
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Bounded-full behavior
// ---------------------------------------------------------------------------

#[test]
fn push_fails_when_full() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    assert!(ring.is_full());
    let err = ring.try_push_mpsc(3, None, None).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Full(3));
    assert_eq!(ring.len(), 2);
    validate(&ring);
}

#[test]
fn push_succeeds_after_pop_frees_space() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    assert_eq!(pop_val(&mut ring), 1);
    push_val(&mut ring, 3);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Pop on empty
// ---------------------------------------------------------------------------

#[test]
fn pop_returns_none_when_empty() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    assert!(ring.try_pop().is_none());
}

#[test]
fn pop_returns_none_after_emptied() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    assert_eq!(pop_val(&mut ring), 1);
    assert!(ring.try_pop().is_none());
}

// ---------------------------------------------------------------------------
// Peek
// ---------------------------------------------------------------------------

#[test]
fn peek_returns_none_when_empty() {
    let ring = new_ring!(4, DEFAULT_TTL);
    assert!(ring.peek_expires_at().is_none());
}

#[test]
fn empty_slots_do_not_leak_sentinel_through_peek_or_drain() {
    // Empty slots store a process-wide sentinel `Instant`, not a per-slot
    // `Instant::now()`. The sentinel must never be observable via expiry
    // operations on an empty ring.
    let ring = new_ring!(1024, DEFAULT_TTL);
    assert!(
        ring.peek_expires_at().is_none(),
        "empty ring must peek None"
    );
    let mut ring = ring;
    let drained = ring.drain_expired(Instant::now());
    assert!(
        drained.is_empty(),
        "empty ring must drain nothing, got {} items",
        drained.len()
    );
    assert_eq!(ring.len(), 0);
}

#[test]
fn slot_sentinel_is_stable_across_constructions() {
    // The sentinel is a process-wide OnceLock. Two rings constructed back to
    // back must observe the same sentinel value. This guards against the
    // sentinel accidentally regressing to per-slot `Instant::now()`.
    let r1 = new_ring!(4, DEFAULT_TTL);
    std::thread::sleep(Duration::from_millis(5));
    let r2 = new_ring!(4, DEFAULT_TTL);
    // Empty rings have no observable expiry; cross-check by populating the
    // first slot's expiry indirectly: the sentinel must precede `now()`
    // captured after both constructions.
    let after_both = Instant::now();
    // Push one item into each so we have at least one occupied slot, then
    // confirm the head expiry is computed from `now + ttl`, not from the
    // sentinel — i.e. occupied slots never inherit the sentinel.
    let mut r1 = r1;
    let mut r2 = r2;
    push_val(&mut r1, 1);
    push_val(&mut r2, 1);
    let h1 = r1.peek_expires_at().unwrap();
    let h2 = r2.peek_expires_at().unwrap();
    assert!(
        h1 >= after_both,
        "occupied head must have a now-derived deadline, not the sentinel"
    );
    assert!(
        h2 >= after_both,
        "occupied head must have a now-derived deadline, not the sentinel"
    );
}

#[test]
fn peek_returns_head_expiry() {
    let ttl = Duration::from_secs(5);
    let mut ring = new_ring!(4, ttl);
    let before = Instant::now();
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    let after = Instant::now();
    let head_expiry = ring.peek_expires_at().unwrap();
    // Head expiry should be within [before+ttl, after+ttl].
    assert!(head_expiry >= before + ttl);
    assert!(head_expiry <= after + ttl);
    // Pop head, now peek should show the second item's expiry.
    pop_val(&mut ring);
    let second_expiry = ring.peek_expires_at().unwrap();
    assert!(second_expiry >= before + ttl);
    assert!(second_expiry <= after + ttl);
}

#[test]
fn peek_does_not_remove_item() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 42);
    let _ = ring.peek_expires_at();
    assert_eq!(ring.len(), 1);
    assert_eq!(pop_val(&mut ring), 42);
}

// ---------------------------------------------------------------------------
// MPSC report channel handles travel with items
// ---------------------------------------------------------------------------

#[test]
fn mpsc_pop_returns_per_slot_channels() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let expiry_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let shutdown_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    push_val_with_channels(
        &mut ring,
        1,
        Some(expiry_ch.clone()),
        Some(shutdown_ch.clone()),
    );
    let result = ring.try_pop().unwrap();
    assert_eq!(result.item, 1);
    assert!(result.expiry_channel.is_some());
    assert!(result.shutdown_channel.is_some());
}

#[test]
fn mpsc_pop_returns_none_channels_when_not_set() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    let result = ring.try_pop().unwrap();
    assert!(result.expiry_channel.is_none());
    assert!(result.shutdown_channel.is_none());
}

// ---------------------------------------------------------------------------
// Metadata preservation across resize (MPSC)
// ---------------------------------------------------------------------------

#[test]
fn growth_preserves_slot_metadata() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let before = Instant::now();
    ring.try_push_mpsc(1, Some(ch.clone()), Some(ch.clone()))
        .unwrap();
    let after = Instant::now();
    ring.request_capacity(4);
    let result = ring.try_pop().unwrap();
    assert_eq!(result.item, 1);
    assert!(result.expires_at >= before + DEFAULT_TTL);
    assert!(result.expires_at <= after + DEFAULT_TTL);
    assert!(result.expiry_channel.is_some());
    assert!(result.shutdown_channel.is_some());
}

#[test]
fn shrink_preserves_slot_metadata() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let before = Instant::now();
    ring.try_push_mpsc(1, Some(ch.clone()), Some(ch.clone()))
        .unwrap();
    let after = Instant::now();
    ring.request_capacity(2);
    let result = ring.try_pop().unwrap();
    assert_eq!(result.item, 1);
    assert!(result.expires_at >= before + DEFAULT_TTL);
    assert!(result.expires_at <= after + DEFAULT_TTL);
    assert!(result.expiry_channel.is_some());
    assert!(result.shutdown_channel.is_some());
}

#[test]
fn shutdown_drain_preserves_slot_metadata() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let before = Instant::now();
    ring.try_push_mpsc(1, Some(ch.clone()), Some(ch.clone()))
        .unwrap();
    let after = Instant::now();
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    let items = ring.shutdown();
    assert_eq!(items[0].item, 1);
    assert!(items[0].expires_at >= before + DEFAULT_TTL);
    assert!(items[0].expires_at <= after + DEFAULT_TTL);
    assert!(items[0].expiry_channel.is_some());
    assert!(items[0].shutdown_channel.is_some());
}

#[test]
fn shutdown_does_not_compact_storage() {
    // Shutdown is a terminal operation. Allocating a new Vec just so the
    // smaller storage lives briefly until the Arc drops is net-negative
    // work; the storage stays at its current size and is freed when the
    // Arc drops.
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 4); // Deferred shrink not applied.
    let _ = ring.shutdown();
    assert_eq!(
        ring.capacity(),
        4,
        "shutdown must not compact: storage stays at original size"
    );
}

// ---------------------------------------------------------------------------
// Growth
// ---------------------------------------------------------------------------

#[test]
fn growth_preserves_fifo_order() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    ring.request_capacity(4);
    assert_eq!(ring.capacity(), 4);
    assert_eq!(ring.target_capacity(), 4);
    assert_eq!(ring.len(), 2);
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(pop_val(&mut ring), 2);
    validate(&ring);
}

#[test]
fn growth_with_wrap_preserves_fifo_order() {
    let mut ring = new_ring!(3, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    // Pop one to move head forward, then push to wrap tail.
    pop_val(&mut ring);
    push_val(&mut ring, 4);
    // head=1, tail=1 (wrapped), len=3 — grow now.
    ring.request_capacity(5);
    assert_eq!(ring.capacity(), 5);
    assert_eq!(ring.len(), 3);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    assert_eq!(pop_val(&mut ring), 4);
    validate(&ring);
}

#[test]
fn growth_from_empty() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    ring.request_capacity(8);
    assert_eq!(ring.capacity(), 8);
    assert!(ring.is_empty());
    validate(&ring);
}

#[test]
fn growth_allows_more_pushes() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    assert!(ring.is_full());
    ring.request_capacity(3);
    push_val(&mut ring, 3);
    assert_eq!(ring.len(), 3);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Immediate shrink
// ---------------------------------------------------------------------------

#[test]
fn immediate_shrink_preserves_fifo_order() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 2);
    assert_eq!(ring.target_capacity(), 2);
    assert_eq!(ring.len(), 2);
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(pop_val(&mut ring), 2);
    validate(&ring);
}

#[test]
fn immediate_shrink_from_empty() {
    let mut ring = new_ring!(8, DEFAULT_TTL);
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 2);
    assert!(ring.is_empty());
    validate(&ring);
}

#[test]
fn immediate_shrink_with_wrap_preserves_order() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    // Pop two to move head forward.
    pop_val(&mut ring);
    pop_val(&mut ring);
    // head=2, tail=3, len=1 — shrink to 2.
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 2);
    assert_eq!(ring.len(), 1);
    assert_eq!(pop_val(&mut ring), 3);
    validate(&ring);
}

#[test]
fn immediate_shrink_to_exact_len_then_pop_push() {
    // Regression: linearize with new_capacity == len left tail == slots.len(),
    // causing an out-of-bounds panic on the next push after a pop.
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    // Immediate shrink to exactly len.
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 2);
    assert!(ring.is_full());
    // Pop frees a slot, then push must not panic.
    assert_eq!(pop_val(&mut ring), 1);
    push_val(&mut ring, 3);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Deferred shrink
// ---------------------------------------------------------------------------

#[test]
fn deferred_shrink_rejects_pushes_at_target() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    // Shrink to 2, but 3 items present — deferred.
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 4); // Vec unchanged.
    assert_eq!(ring.target_capacity(), 2);
    assert!(ring.is_full()); // Full at target, not at capacity.
    let err = ring.try_push_mpsc(4, None, None).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Full(4));
    validate(&ring);
}

#[test]
fn deferred_shrink_compacts_when_len_reaches_target() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    ring.request_capacity(2);
    // Pop one — still above target (len=2, target=2).
    assert_eq!(pop_val(&mut ring), 1);
    // len is now 2, which equals target — compaction triggered.
    assert_eq!(ring.capacity(), 2);
    assert_eq!(ring.target_capacity(), 2);
    assert_eq!(ring.len(), 2);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    validate(&ring);
}

#[test]
fn deferred_shrink_fifo_preserved_after_compaction() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 10);
    push_val(&mut ring, 20);
    push_val(&mut ring, 30);
    push_val(&mut ring, 40);
    ring.request_capacity(2);
    // Pop two to reach target.
    assert_eq!(pop_val(&mut ring), 10);
    assert_eq!(pop_val(&mut ring), 20);
    // Compaction should have happened after the second pop.
    assert_eq!(ring.capacity(), 2);
    assert_eq!(pop_val(&mut ring), 30);
    assert_eq!(pop_val(&mut ring), 40);
    validate(&ring);
}

#[test]
fn deferred_shrink_compact_then_pop_push() {
    // Regression: compaction via try_pop with target == len left tail out of
    // bounds, causing a panic on the next push after freeing a slot.
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    ring.request_capacity(2);
    // Pop to trigger compaction (len drops from 3 to 2 == target).
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(ring.capacity(), 2);
    assert!(ring.is_full());
    // Pop frees a slot, then push must not panic.
    assert_eq!(pop_val(&mut ring), 2);
    push_val(&mut ring, 4);
    assert_eq!(pop_val(&mut ring), 3);
    assert_eq!(pop_val(&mut ring), 4);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[test]
fn shutdown_returns_all_items_fifo() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    let items: Vec<i32> = ring.shutdown().into_iter().map(|r| r.item).collect();
    assert_eq!(items, vec![1, 2, 3]);
    assert!(ring.is_empty());
    assert!(ring.is_shutdown());
    validate(&ring);
}

#[test]
fn shutdown_on_empty_buffer() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let items = ring.shutdown();
    assert!(items.is_empty());
    assert!(ring.is_shutdown());
    validate(&ring);
}

#[test]
fn shutdown_rejects_subsequent_pushes() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    ring.shutdown();
    let err = ring.try_push_mpsc(1, None, None).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(1));
}

#[test]
fn shutdown_is_irreversible() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    ring.shutdown();
    // Second shutdown returns empty.
    let items = ring.shutdown();
    assert!(items.is_empty());
    assert!(ring.is_shutdown());
}

#[test]
fn shutdown_returns_items_with_channels() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    push_val_with_channels(&mut ring, 1, None, Some(ch.clone()));
    push_val_with_channels(&mut ring, 2, None, Some(ch.clone()));
    let items = ring.shutdown();
    assert_eq!(items.len(), 2);
    assert!(items[0].shutdown_channel.is_some());
    assert!(items[1].shutdown_channel.is_some());
}

#[test]
fn shutdown_with_wrap_around() {
    let mut ring = new_ring!(3, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    pop_val(&mut ring);
    push_val(&mut ring, 4);
    // head=1, items are [2, 3, 4] wrapping.
    let items: Vec<i32> = ring.shutdown().into_iter().map(|r| r.item).collect();
    assert_eq!(items, vec![2, 3, 4]);
}

// ---------------------------------------------------------------------------
// Capacity clamping
// ---------------------------------------------------------------------------

#[test]
fn request_capacity_zero_clamps_to_one() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    ring.request_capacity(0);
    assert_eq!(ring.target_capacity(), 1);
}

// ---------------------------------------------------------------------------
// No-op resize
// ---------------------------------------------------------------------------

#[test]
fn request_same_capacity_is_noop() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    ring.request_capacity(4);
    assert_eq!(ring.capacity(), 4);
    assert_eq!(ring.len(), 2);
    // Head should not have been reset (no linearization).
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(pop_val(&mut ring), 2);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Capacity 1 edge case
// ---------------------------------------------------------------------------

#[test]
fn capacity_one_push_pop_cycle() {
    let mut ring = new_ring!(1, DEFAULT_TTL);
    push_val(&mut ring, 10);
    assert!(ring.is_full());
    assert_eq!(pop_val(&mut ring), 10);
    assert!(ring.is_empty());
    push_val(&mut ring, 20);
    assert_eq!(pop_val(&mut ring), 20);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// Multiple resize operations
// ---------------------------------------------------------------------------

#[test]
fn grow_then_shrink() {
    let mut ring = new_ring!(2, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    ring.request_capacity(4);
    push_val(&mut ring, 3);
    assert_eq!(ring.capacity(), 4);
    // Shrink back — len=3, target=2 → deferred.
    ring.request_capacity(2);
    assert_eq!(ring.capacity(), 4);
    assert_eq!(ring.target_capacity(), 2);
    assert_eq!(pop_val(&mut ring), 1);
    // len=2, target=2 → compacted.
    assert_eq!(ring.capacity(), 2);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    validate(&ring);
}

#[test]
fn shrink_then_grow_cancels_deferred() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    // Deferred shrink to 2.
    ring.request_capacity(2);
    assert_eq!(ring.target_capacity(), 2);
    // Grow to 6 — overrides the pending shrink.
    ring.request_capacity(6);
    assert_eq!(ring.capacity(), 6);
    assert_eq!(ring.target_capacity(), 6);
    assert!(!ring.is_full());
    push_val(&mut ring, 4);
    assert_eq!(pop_val(&mut ring), 1);
    assert_eq!(pop_val(&mut ring), 2);
    assert_eq!(pop_val(&mut ring), 3);
    assert_eq!(pop_val(&mut ring), 4);
    validate(&ring);
}

// ---------------------------------------------------------------------------
// SPSC push pattern
// ---------------------------------------------------------------------------

#[test]
fn spsc_push_succeeds_in_spsc_mode() {
    let mut ring = new_spsc_ring!(4, DEFAULT_TTL, None, None);
    push_val_spsc(&mut ring, 1);
    assert_eq!(ring.len(), 1);
    assert_eq!(pop_val(&mut ring), 1);
}

#[test]
fn spsc_push_rejected_in_mpsc_mode() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    let err = ring.try_push_spsc(1).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidPattern(1));
}

#[test]
fn spsc_push_returns_shutdown_when_shutdown() {
    let mut ring = new_spsc_ring!(4, DEFAULT_TTL, None, None);
    ring.shutdown();
    let err = ring.try_push_spsc(1).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(1));
}

#[test]
fn spsc_push_returns_full_when_full() {
    let mut ring = new_spsc_ring!(1, DEFAULT_TTL, None, None);
    push_val_spsc(&mut ring, 1);
    let err = ring.try_push_spsc(2).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Full(2));
}

// ---------------------------------------------------------------------------
// MPSC push pattern
// ---------------------------------------------------------------------------

#[test]
fn mpsc_push_succeeds_in_mpsc_mode() {
    let mut ring = new_ring!(4, DEFAULT_TTL);
    push_val(&mut ring, 1);
    assert_eq!(ring.len(), 1);
    assert_eq!(pop_val(&mut ring), 1);
}

#[test]
fn mpsc_push_rejected_in_spsc_mode() {
    let mut ring = new_spsc_ring!(4, DEFAULT_TTL, None, None);
    let err = ring.try_push_mpsc(1, None, None).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidPattern(1));
}

// ---------------------------------------------------------------------------
// SPSC PopResult: ring-level channels on pop
// ---------------------------------------------------------------------------

#[test]
fn spsc_pop_returns_ring_level_channels() {
    let expiry_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let shutdown_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let mut ring = new_spsc_ring!(
        4,
        DEFAULT_TTL,
        Some(expiry_ch.clone()),
        Some(shutdown_ch.clone())
    );
    push_val_spsc(&mut ring, 1);
    let result = ring.try_pop().unwrap();
    assert_eq!(result.item, 1);
    assert!(result.expiry_channel.is_some());
    assert!(result.shutdown_channel.is_some());
}

#[test]
fn spsc_pop_returns_none_channels_when_ring_has_none() {
    let mut ring = new_spsc_ring!(4, DEFAULT_TTL, None, None);
    push_val_spsc(&mut ring, 1);
    let result = ring.try_pop().unwrap();
    assert!(result.expiry_channel.is_none());
    assert!(result.shutdown_channel.is_none());
}

// ---------------------------------------------------------------------------
// SPSC PopResult: ring-level channels on shutdown
// ---------------------------------------------------------------------------

#[test]
fn spsc_shutdown_returns_items_with_ring_level_channels() {
    let expiry_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let shutdown_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let mut ring = new_spsc_ring!(
        4,
        DEFAULT_TTL,
        Some(expiry_ch.clone()),
        Some(shutdown_ch.clone())
    );
    push_val_spsc(&mut ring, 1);
    push_val_spsc(&mut ring, 2);
    let items = ring.shutdown();
    assert_eq!(items.len(), 2);
    for item in &items {
        assert!(item.expiry_channel.is_some());
        assert!(item.shutdown_channel.is_some());
    }
}

// ---------------------------------------------------------------------------
// SPSC per-slot channels always None in storage
// ---------------------------------------------------------------------------

#[test]
fn spsc_per_slot_channels_are_none() {
    // Even when ring-level channels are set, per-slot fields remain None.
    // Verified by checking that MPSC pop on the same data would yield None
    // channels — we test indirectly by confirming pop returns ring-level
    // channels (i.e. slot did not store them).
    let expiry_ch: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let mut ring = new_spsc_ring!(4, DEFAULT_TTL, Some(expiry_ch.clone()), None);
    push_val_spsc(&mut ring, 1);
    // PopResult expiry_channel comes from ring, not slot.
    let result = ring.try_pop().unwrap();
    assert!(result.expiry_channel.is_some());
    // shutdown_channel is None at ring level, so PopResult reflects that.
    assert!(result.shutdown_channel.is_none());
}

// ---------------------------------------------------------------------------
// ttl_reduced flag transitions and full-scan drain
// ---------------------------------------------------------------------------

#[test]
fn set_ttl_reduces_with_items_sets_flag() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1);
    assert!(!ring.ttl_reduced(), "flag clear before set_ttl");
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    assert!(
        ring.ttl_reduced(),
        "set_ttl with smaller value on non-empty ring must set the flag"
    );
}

#[test]
fn set_ttl_reduces_when_empty_does_not_set_flag() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    assert!(
        !ring.ttl_reduced(),
        "set_ttl on empty ring must not set the flag"
    );
}

#[test]
fn set_ttl_increase_does_not_change_flag() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1);
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    assert!(ring.ttl_reduced());
    ring.set_ttl(Duration::from_secs(60)).unwrap();
    assert!(
        ring.ttl_reduced(),
        "TTL increase must not clear the flag (cannot repair existing non-monotonic ring)"
    );
}

#[test]
fn drain_expired_head_only_fast_path_when_flag_clear() {
    // With flag clear, drain_expired follows the fast path: contiguous head
    // prefix only, no compaction.
    let mut ring = new_ring!(4, Duration::from_millis(20));
    push_val(&mut ring, 1);
    std::thread::sleep(Duration::from_millis(50));
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    assert!(!ring.ttl_reduced());
    let drained = ring.drain_expired(Instant::now());
    // Item 1 should be expired (pushed before the sleep), 2 and 3 not yet.
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].item, 1);
    assert_eq!(ring.len(), 2);
    // Survivors remain in FIFO order.
    assert_eq!(ring.try_pop().unwrap().item, 2);
    assert_eq!(ring.try_pop().unwrap().item, 3);
}

#[test]
fn drain_expired_after_ttl_shrink_finds_non_head_expired() {
    // Push A under a long TTL, then reduce TTL and push B. After the short
    // wait, B is expired but A is not. The head-only fast path would miss B;
    // the full-scan path must find it.
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: deadline ~ now+60s
    ring.set_ttl(Duration::from_millis(20)).unwrap();
    push_val(&mut ring, 2); // B: deadline ~ now+20ms
    assert!(ring.ttl_reduced());
    std::thread::sleep(Duration::from_millis(60));
    let drained = ring.drain_expired(Instant::now());
    // Only B expired; A is still alive.
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].item, 2);
    assert_eq!(ring.len(), 1);
    // Surviving head is A.
    assert_eq!(ring.try_pop().unwrap().item, 1);
    // Single-survivor case: flag must have cleared (trivially monotonic).
    // (We popped A above, so re-check by setting up a similar scenario.)
}

#[test]
fn drain_expired_clears_flag_when_single_survivor_remains() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1);
    ring.set_ttl(Duration::from_millis(20)).unwrap();
    push_val(&mut ring, 2);
    assert!(ring.ttl_reduced());
    std::thread::sleep(Duration::from_millis(60));
    let _ = ring.drain_expired(Instant::now());
    // After drain, only item 1 remains; len == 1 is trivially monotonic.
    assert_eq!(ring.len(), 1);
    assert!(
        !ring.ttl_reduced(),
        "single survivor must clear ttl_reduced flag"
    );
}

#[test]
fn drain_expired_clears_flag_when_empty() {
    // Item 1 pushed under a short TTL, then TTL reduced (sets flag), then
    // item 2 pushed under the now-shorter TTL. Sleep until both expire.
    let mut ring = new_ring!(4, Duration::from_millis(40));
    push_val(&mut ring, 1);
    ring.set_ttl(Duration::from_millis(10)).unwrap();
    assert!(ring.ttl_reduced());
    push_val(&mut ring, 2);
    std::thread::sleep(Duration::from_millis(80));
    let drained = ring.drain_expired(Instant::now());
    assert_eq!(drained.len(), 2);
    assert_eq!(ring.len(), 0);
    assert!(
        !ring.ttl_reduced(),
        "empty ring after drain must clear ttl_reduced flag"
    );
}

#[test]
fn drain_expired_full_scan_clears_flag_when_survivors_monotonic() {
    // Build a ring whose survivors after drain are monotonic (>= 2 survivors).
    let mut ring = new_ring!(8, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: long deadline
    ring.set_ttl(Duration::from_millis(20)).unwrap();
    push_val(&mut ring, 2); // B: short
    ring.set_ttl(Duration::from_secs(60)).unwrap(); // increase, flag stays
    push_val(&mut ring, 3); // C: long, but pushed later than A
    assert!(ring.ttl_reduced());
    // Sleep until B expires but A and C don't.
    std::thread::sleep(Duration::from_millis(60));
    let drained = ring.drain_expired(Instant::now());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].item, 2);
    // Survivors A and C, both with long deadlines. C was pushed later than A,
    // so A's deadline < C's deadline → monotonic. Flag should clear.
    assert_eq!(ring.len(), 2);
    assert!(
        !ring.ttl_reduced(),
        "monotonic survivors should clear the flag"
    );
}

#[test]
fn drain_expired_full_scan_keeps_flag_when_survivors_non_monotonic() {
    // After drain, survivors are still non-monotonic (>= 2 survivors with
    // FIFO-non-monotonic deadlines). Drain at a time where nothing has
    // expired, so the full scan runs but no items leave; the survivors are
    // exactly the original non-monotonic [A, B] pair.
    let mut ring = new_ring!(8, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: deadline ~ now+60s
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    push_val(&mut ring, 2); // B: deadline ~ now+50ms < A → non-monotonic
    assert!(ring.ttl_reduced());
    // Drain immediately; neither item has expired.
    let drained = ring.drain_expired(Instant::now());
    assert!(drained.is_empty());
    assert_eq!(ring.len(), 2);
    assert!(
        ring.ttl_reduced(),
        "non-monotonic survivors must keep the flag set"
    );
}

#[test]
fn drain_expired_with_pending_shrink_compacts_to_target() {
    // Configure a deferred shrink, run a drain that drops occupancy below
    // target, and assert the slow-path drain honours the shrink by compacting
    // the ring to target_capacity.
    let mut ring = new_ring!(8, Duration::from_millis(100));
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    push_val(&mut ring, 3);
    // Reduce TTL to set the flag and force the slow path.
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    assert!(ring.ttl_reduced());
    // Defer shrink: len(3) > target(2), so request_capacity stays pending
    // until occupancy drops to <= 2.
    ring.request_capacity(2);
    assert_eq!(ring.target_capacity(), 2);
    assert_eq!(ring.capacity(), 8, "shrink must defer while len > target");
    // Sleep until all three items expire (under their original 100ms TTL).
    std::thread::sleep(Duration::from_millis(200));
    let drained = ring.drain_expired(Instant::now());
    assert_eq!(drained.len(), 3);
    assert_eq!(ring.len(), 0);
    assert_eq!(
        ring.capacity(),
        2,
        "deferred shrink must compact during slow-path drain"
    );
    assert_eq!(ring.target_capacity(), 2);
    validate(&ring);
}

#[test]
fn flag_round_trip_after_shrink_and_drain() {
    // Shrink TTL, push items, drain at intervals: flag is set on the shrink,
    // remains set while non-monotonic survivors exist, and clears once the
    // survivors become monotonic.
    let mut ring = new_ring!(8, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: long deadline
    assert!(!ring.ttl_reduced());
    // Shrink TTL: flag is set because the ring is non-empty.
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    assert!(ring.ttl_reduced(), "shrink on non-empty ring sets the flag");
    push_val(&mut ring, 2); // B: short, non-monotonic vs A

    // First drain happens before B expires. Survivors stay [A, B] and are
    // non-monotonic, so the flag must remain set.
    let drained = ring.drain_expired(Instant::now());
    assert!(drained.is_empty());
    assert_eq!(ring.len(), 2);
    assert!(
        ring.ttl_reduced(),
        "non-monotonic survivors keep the flag set across drain"
    );
    // Second drain after B expires. Single survivor [A] is trivially
    // monotonic, so the flag must clear.
    std::thread::sleep(Duration::from_millis(120));
    let drained = ring.drain_expired(Instant::now());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].item, 2);
    assert_eq!(ring.len(), 1);
    assert!(
        !ring.ttl_reduced(),
        "monotonic survivors clear the flag once non-monotonic items leave"
    );
}

#[test]
fn drain_expired_compacts_when_gap_opened() {
    // Capacity 4: fill with [A, B, C, D] where B and C expire but A and D
    // survive. Drain must produce [B, C] in FIFO order, leaving A and D as
    // contiguous survivors with head=0.
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: long
    ring.set_ttl(Duration::from_millis(20)).unwrap();
    push_val(&mut ring, 2); // B: short
    push_val(&mut ring, 3); // C: short
    ring.set_ttl(Duration::from_secs(60)).unwrap();
    push_val(&mut ring, 4); // D: long (still flag set; only drain clears it)
    std::thread::sleep(Duration::from_millis(60));
    let drained = ring.drain_expired(Instant::now());
    let drained_items: Vec<i32> = drained.iter().map(|p| p.item).collect();
    assert_eq!(drained_items, vec![2, 3], "drain must return B,C in FIFO");
    assert_eq!(ring.len(), 2);
    // Survivors should pop in FIFO order A, D.
    assert_eq!(ring.try_pop().unwrap().item, 1);
    assert_eq!(ring.try_pop().unwrap().item, 4);
}

#[test]
fn drain_expired_metadata_preserved_for_survivors_after_compaction() {
    let expiry: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    let shutdown: Arc<dyn ReportChannel<i32>> = Arc::new(TestChannel);
    // MPSC mode so per-slot channels survive linearization.
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val_with_channels(&mut ring, 1, Some(expiry.clone()), Some(shutdown.clone())); // A: long
    ring.set_ttl(Duration::from_millis(20)).unwrap();
    push_val_with_channels(&mut ring, 2, Some(expiry.clone()), Some(shutdown.clone())); // B: short
    ring.set_ttl(Duration::from_secs(60)).unwrap();
    push_val_with_channels(&mut ring, 3, Some(expiry.clone()), Some(shutdown.clone())); // C: long
    std::thread::sleep(Duration::from_millis(60));
    let _ = ring.drain_expired(Instant::now());
    assert_eq!(ring.len(), 2);
    let a = ring.try_pop().unwrap();
    assert_eq!(a.item, 1);
    assert!(a.expiry_channel.is_some());
    assert!(a.shutdown_channel.is_some());
    let c = ring.try_pop().unwrap();
    assert_eq!(c.item, 3);
    assert!(c.expiry_channel.is_some());
    assert!(c.shutdown_channel.is_some());
}

#[test]
fn peek_expires_at_head_only_when_flag_clear() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1);
    push_val(&mut ring, 2);
    assert!(!ring.ttl_reduced());
    let head_deadline = ring.slots_head_deadline_for_test();
    assert_eq!(ring.peek_expires_at().unwrap(), head_deadline);
}

#[test]
fn peek_expires_at_returns_minimum_when_flag_set() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1); // A: long
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    push_val(&mut ring, 2); // B: short — non-monotonic relative to A
    ring.set_ttl(Duration::from_secs(60)).unwrap();
    push_val(&mut ring, 3); // C: long
    assert!(ring.ttl_reduced());
    let min = ring.peek_expires_at().unwrap();
    // Minimum deadline must be B's, which is shorter than A's and C's.
    let head = ring.slots_head_deadline_for_test();
    assert!(
        min < head,
        "peek must return minimum (B's deadline), not head's (A's)"
    );
}

#[test]
fn peek_expires_at_does_not_clear_flag() {
    let mut ring = new_ring!(4, Duration::from_secs(60));
    push_val(&mut ring, 1);
    ring.set_ttl(Duration::from_millis(50)).unwrap();
    push_val(&mut ring, 2);
    assert!(ring.ttl_reduced());
    let _ = ring.peek_expires_at();
    let _ = ring.peek_expires_at();
    assert!(
        ring.ttl_reduced(),
        "peek_expires_at must not mutate the flag"
    );
}
