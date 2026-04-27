// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caducus::{
    CaducusErrorKind, MpscBuilder, MpscSender, Receiver, ReportChannel, SpscBuilder, SpscSender,
};

// ---------------------------------------------------------------------------
// Test ReportChannel implementation
// ---------------------------------------------------------------------------

struct CollectorChannel<T: Send + 'static> {
    items: Mutex<Vec<T>>,
}

impl<T: Send + 'static> CollectorChannel<T> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            items: Mutex::new(Vec::new()),
        })
    }

    fn items(&self) -> Vec<T>
    where
        T: Clone,
    {
        self.items.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }
}

impl<T: Send + 'static> ReportChannel<T> for CollectorChannel<T> {
    fn send(&self, item: T) -> Result<(), T> {
        self.items.lock().unwrap().push(item);
        Ok(())
    }
}

/// A channel that always rejects.
struct RejectChannel;

impl<T: Send + 'static> ReportChannel<T> for RejectChannel {
    fn send(&self, item: T) -> Result<(), T> {
        Err(item)
    }
}

/// A channel that panics on send.
struct PanicChannel;

impl<T: Send + 'static> ReportChannel<T> for PanicChannel {
    fn send(&self, _item: T) -> Result<(), T> {
        panic!("PanicChannel: intentional panic in ReportChannel::send");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DEFAULT_TTL: Duration = Duration::from_secs(5);
const SHORT_TTL: Duration = Duration::from_millis(50);

#[allow(clippy::type_complexity)]
fn spsc_channel(
    capacity: usize,
    ttl: Duration,
) -> (
    SpscSender<i32>,
    Receiver<i32>,
    Arc<CollectorChannel<i32>>,
    Arc<CollectorChannel<i32>>,
) {
    let expiry = CollectorChannel::new();
    let shutdown = CollectorChannel::new();
    let (tx, rx) = SpscBuilder::new(capacity, ttl)
        .expiry_channel(expiry.clone())
        .shutdown_channel(shutdown.clone())
        .build()
        .expect("build should succeed");
    (tx, rx, expiry, shutdown)
}

fn mpsc_channel(capacity: usize, ttl: Duration) -> (MpscSender<i32>, Receiver<i32>) {
    let (tx, rx) = MpscBuilder::new(capacity, ttl)
        .build()
        .expect("build should succeed");
    (tx, rx)
}

#[allow(clippy::type_complexity)]
fn mpsc_channel_with_reports(
    capacity: usize,
    ttl: Duration,
) -> (
    MpscSender<i32>,
    Receiver<i32>,
    Arc<CollectorChannel<i32>>,
    Arc<CollectorChannel<i32>>,
) {
    let expiry = CollectorChannel::new();
    let shutdown = CollectorChannel::new();
    let (tx, rx) = MpscBuilder::new(capacity, ttl)
        .expiry_channel(expiry.clone())
        .shutdown_channel(shutdown.clone())
        .build()
        .expect("build should succeed");
    (tx, rx, expiry, shutdown)
}

// ===========================================================================
// SPSC: basic send/receive
// ===========================================================================

#[tokio::test]
async fn spsc_send_and_receive() {
    let (tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    tx.send(42).unwrap();
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 42);
}

#[tokio::test]
async fn spsc_fifo_ordering() {
    let (tx, rx, _, _) = spsc_channel(8, DEFAULT_TTL);
    for i in 0..5 {
        tx.send(i).unwrap();
    }
    for i in 0..5 {
        let val = rx
            .next(Some(Instant::now() + Duration::from_millis(100)))
            .await
            .unwrap();
        assert_eq!(val, i);
    }
}

#[tokio::test]
async fn spsc_bounded_full() {
    let (tx, _rx, _, _) = spsc_channel(2, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    let err = tx.send(3).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Full(3));
}

#[tokio::test]
async fn spsc_full_returns_item() {
    let (tx, _rx, _, _) = spsc_channel(1, DEFAULT_TTL);
    tx.send(1).unwrap();
    let err = tx.send(99).unwrap_err();
    assert_eq!(err.into_inner(), Some(99));
}

// ===========================================================================
// SPSC: timeout
// ===========================================================================

#[tokio::test]
async fn spsc_receive_timeout() {
    let (_tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
}

#[tokio::test]
async fn spsc_default_timeout_on_none() {
    let (_tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    let start = std::time::Instant::now();
    let err = rx.next(None).await.unwrap_err();
    let elapsed = start.elapsed();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
    // Default is 1 second; allow some slack.
    assert!(elapsed >= Duration::from_millis(900));
    assert!(elapsed <= Duration::from_millis(1500));
}

// ===========================================================================
// SPSC: shutdown
// ===========================================================================

#[tokio::test]
async fn spsc_sender_shutdown() {
    let (tx, rx, _, shutdown) = spsc_channel(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    tx.shutdown();
    // Sender can't send after shutdown.
    let err = tx.send(3).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(3));
    // Receiver gets shutdown.
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
    // Shutdown channel received the drained items.
    assert_eq!(shutdown.len(), 2);
    let items = shutdown.items();
    assert_eq!(items, vec![1, 2]);
}

#[tokio::test]
async fn spsc_receiver_drop_triggers_shutdown() {
    let (tx, rx, _, shutdown) = spsc_channel(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    drop(rx);
    // Sender should see shutdown.
    assert!(tx.is_closed());
    let err = tx.send(2).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(2));
    // Items drained during receiver drop reported through shutdown channel.
    assert_eq!(shutdown.len(), 1);
}

// ===========================================================================
// SPSC: sender drop triggers shutdown
// ===========================================================================

#[tokio::test]
async fn spsc_sender_drop_triggers_shutdown() {
    let (tx, rx, _, shutdown) = spsc_channel(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    drop(tx);
    // Receiver should see shutdown.
    assert!(rx.is_closed());
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
    // Items drained during sender drop reported through shutdown channel.
    assert_eq!(shutdown.items(), vec![1, 2]);
}

// ===========================================================================
// SPSC: expiry
// ===========================================================================

#[tokio::test]
async fn spsc_expired_items_not_returned_to_receiver() {
    let (tx, rx, expiry, _) = spsc_channel(4, SHORT_TTL);
    tx.send(1).unwrap();
    // Wait for expiry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Receiver should not get expired item.
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
    // Reclaimer should have reported it.
    // Give reclaimer a moment to process.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(expiry.len(), 1);
    assert_eq!(expiry.items(), vec![1]);
}

#[tokio::test]
async fn spsc_live_items_returned_after_expired() {
    let (tx, rx, _, _) = spsc_channel(4, SHORT_TTL);
    tx.send(1).unwrap(); // will expire

    // Update TTL so next item lives.
    tx.update_ttl(DEFAULT_TTL).unwrap();
    tx.send(2).unwrap();

    // Wait for item 1 to expire.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Receiver should get item 2.
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(val, 2);
}

// ===========================================================================
// SPSC: TTL and capacity updates
// ===========================================================================

#[tokio::test]
async fn spsc_update_ttl() {
    let (tx, _rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    tx.update_ttl(Duration::from_secs(10)).unwrap();
    // Invalid TTL rejected.
    let err = tx.update_ttl(Duration::ZERO).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn spsc_update_capacity() {
    let (tx, _rx, _, _) = spsc_channel(2, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    // Full at 2.
    assert!(tx.send(3).is_err());
    // Grow.
    tx.update_capacity(4);
    tx.send(3).unwrap();
    tx.send(4).unwrap();
}

// ===========================================================================
// SPSC: optional report channels (alignment with MPSC)
// ===========================================================================

#[tokio::test]
async fn spsc_no_channels_silently_drops_on_expiry() {
    // Builder with no expiry/shutdown channels: expired items must be dropped
    // silently without panic, matching MPSC's None-channel semantics.
    let (tx, rx) = SpscBuilder::<i32>::new(4, SHORT_TTL)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
}

#[tokio::test]
async fn spsc_no_channels_silently_drops_on_shutdown() {
    let (tx, rx) = SpscBuilder::<i32>::new(4, DEFAULT_TTL)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    tx.shutdown();
    // Receiver immediately observes shutdown without items.
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
}

#[tokio::test]
async fn spsc_only_expiry_channel_routes_expired_drops_shutdown() {
    let expiry = CollectorChannel::new();
    let (tx, _rx) = SpscBuilder::new(4, SHORT_TTL)
        .expiry_channel(expiry.clone())
        .build()
        .expect("build should succeed");
    tx.send(7).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Force the reclaimer to run by sending another item and waiting briefly.
    let _ = tx.send(8);
    tokio::time::sleep(Duration::from_millis(20)).await;
    let collected = expiry.items();
    assert!(
        collected.contains(&7),
        "expected expiry channel to receive 7, got {collected:?}"
    );
    // Shutdown drops silently.
    tx.shutdown();
}

#[tokio::test]
async fn spsc_only_shutdown_channel_drops_expiry() {
    let shutdown = CollectorChannel::new();
    let (tx, _rx) = SpscBuilder::new(4, DEFAULT_TTL)
        .shutdown_channel(shutdown.clone())
        .build()
        .expect("build should succeed");
    tx.send(11).unwrap();
    tx.shutdown();
    let collected = shutdown.items();
    assert!(
        collected.contains(&11),
        "expected shutdown channel to receive 11, got {collected:?}"
    );
}

// ===========================================================================
// SPSC: report channel failure
// ===========================================================================

#[tokio::test]
async fn spsc_failed_expiry_report_drops_item() {
    let reject: Arc<dyn ReportChannel<i32>> = Arc::new(RejectChannel);
    let shutdown = CollectorChannel::new();
    let (tx, rx, ..) = SpscBuilder::new(4, SHORT_TTL)
        .expiry_channel(reject)
        .shutdown_channel(shutdown)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    // Wait for expiry + reclaimer.
    tokio::time::sleep(Duration::from_millis(150)).await;
    // Item expired but report failed — receiver still shouldn't get it.
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
}

#[tokio::test]
async fn spsc_failed_shutdown_report_drops_item() {
    let expiry = CollectorChannel::new();
    let reject: Arc<dyn ReportChannel<i32>> = Arc::new(RejectChannel);
    let (tx, _rx, ..) = SpscBuilder::new(4, DEFAULT_TTL)
        .expiry_channel(expiry)
        .shutdown_channel(reject)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    // Shutdown — report will fail but shouldn't panic.
    tx.shutdown();
}

// ===========================================================================
// SPSC: panicking report channel containment
// ===========================================================================

#[tokio::test]
async fn spsc_panicking_expiry_channel_does_not_kill_reclaimer() {
    let panic_ch: Arc<dyn ReportChannel<i32>> = Arc::new(PanicChannel);
    let shutdown = CollectorChannel::new();
    let (tx, rx, ..) = SpscBuilder::new(4, SHORT_TTL)
        .expiry_channel(panic_ch)
        .shutdown_channel(shutdown)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    // Wait for expiry — reclaimer calls the panicking channel.
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Reclaimer should still be alive. Send another item with long TTL.
    tx.update_ttl(DEFAULT_TTL).unwrap();
    tx.send(2).unwrap();
    // Receiver should get item 2 — reclaimer survived the panic.
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(500)))
        .await
        .unwrap();
    assert_eq!(val, 2);
}

#[tokio::test]
async fn spsc_panicking_shutdown_channel_in_sender_drop() {
    let expiry = CollectorChannel::new();
    let panic_ch: Arc<dyn ReportChannel<i32>> = Arc::new(PanicChannel);
    let (tx, rx, ..) = SpscBuilder::new(4, DEFAULT_TTL)
        .expiry_channel(expiry)
        .shutdown_channel(panic_ch)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    // Sender drop calls shutdown → report_shutdown → panicking channel.
    // Must not abort the process.
    drop(tx);
    // Receiver sees shutdown.
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
}

#[tokio::test]
async fn spsc_panicking_shutdown_channel_in_receiver_drop() {
    let expiry = CollectorChannel::new();
    let panic_ch: Arc<dyn ReportChannel<i32>> = Arc::new(PanicChannel);
    let (tx, rx, ..) = SpscBuilder::new(4, DEFAULT_TTL)
        .expiry_channel(expiry)
        .shutdown_channel(panic_ch)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    // Receiver drop calls shutdown → report_shutdown → panicking channel.
    // Must not abort the process.
    drop(rx);
    // Sender sees shutdown.
    assert!(tx.is_closed());
    let err = tx.send(2).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(2));
}

// ===========================================================================
// SPSC: is_closed
// ===========================================================================

#[tokio::test]
async fn spsc_is_closed() {
    let (tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    assert!(!tx.is_closed());
    assert!(!rx.is_closed());
    tx.shutdown();
    assert!(tx.is_closed());
    assert!(rx.is_closed());
}

// ===========================================================================
// MPSC: basic send/receive
// ===========================================================================

#[tokio::test]
async fn mpsc_send_and_receive() {
    let (tx, rx) = mpsc_channel(4, DEFAULT_TTL);
    tx.send(42).unwrap();
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 42);
}

#[tokio::test]
async fn mpsc_fifo_ordering() {
    let (tx, rx) = mpsc_channel(8, DEFAULT_TTL);
    for i in 0..5 {
        tx.send(i).unwrap();
    }
    for i in 0..5 {
        let val = rx
            .next(Some(Instant::now() + Duration::from_millis(100)))
            .await
            .unwrap();
        assert_eq!(val, i);
    }
}

#[tokio::test]
async fn mpsc_bounded_full() {
    let (tx, _rx) = mpsc_channel(2, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    let err = tx.send(3).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Full(3));
}

// ===========================================================================
// MPSC: clone semantics
// ===========================================================================

#[tokio::test]
async fn mpsc_clone_sends_to_same_channel() {
    let (tx, rx) = mpsc_channel(4, DEFAULT_TTL);
    let tx2 = tx.clone();
    tx.send(1).unwrap();
    tx2.send(2).unwrap();
    let v1 = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    let v2 = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(v1, 1);
    assert_eq!(v2, 2);
}

#[tokio::test]
async fn mpsc_clone_captures_channel_snapshot() {
    let expiry_a = CollectorChannel::<i32>::new();
    let expiry_b = CollectorChannel::<i32>::new();
    let (tx, rx, _, _) = mpsc_channel_with_reports(4, SHORT_TTL);
    // Set channel A on original.
    tx.set_expiry_channel(Some(expiry_a.clone()));
    // Clone captures channel A.
    let tx2 = tx.clone();
    // Change original to channel B — clone keeps A.
    tx.set_expiry_channel(Some(expiry_b.clone()));

    tx.send(10).unwrap(); // uses channel B
    tx2.send(20).unwrap(); // uses channel A

    // Wait for expiry.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Item 10 reported to B, item 20 reported to A.
    // Give reclaimer time.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(expiry_a.items(), vec![20]);
    assert_eq!(expiry_b.items(), vec![10]);
    drop(rx);
}

// ===========================================================================
// MPSC: shutdown
// ===========================================================================

#[tokio::test]
async fn mpsc_sender_shutdown_with_channels() {
    let (tx, rx, _, shutdown) = mpsc_channel_with_reports(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    tx.shutdown();
    let err = tx.send(3).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(3));
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
    assert_eq!(shutdown.items(), vec![1, 2]);
}

#[tokio::test]
async fn mpsc_receiver_drop_triggers_shutdown() {
    let (tx, rx, _, shutdown) = mpsc_channel_with_reports(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    drop(rx);
    assert!(tx.is_closed());
    let err = tx.send(2).unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(2));
    assert_eq!(shutdown.len(), 1);
}

// ===========================================================================
// MPSC: sender drop triggers shutdown
// ===========================================================================

#[tokio::test]
async fn mpsc_last_sender_drop_triggers_shutdown() {
    let (tx, rx, _, shutdown) = mpsc_channel_with_reports(4, DEFAULT_TTL);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    drop(tx);
    // Receiver should see shutdown.
    assert!(rx.is_closed());
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Shutdown(()));
    // Items drained during last sender drop reported through shutdown channel.
    assert_eq!(shutdown.items(), vec![1, 2]);
}

#[tokio::test]
async fn mpsc_clone_drop_does_not_trigger_shutdown() {
    let (tx, rx, _, _) = mpsc_channel_with_reports(4, DEFAULT_TTL);
    let tx2 = tx.clone();
    tx2.send(1).unwrap();
    drop(tx2);
    // Channel should still be open — original sender alive.
    assert!(!tx.is_closed());
    assert!(!rx.is_closed());
    tx.send(2).unwrap();
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 1);
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 2);
}

// ===========================================================================
// MPSC: expiry with per-sender channels
// ===========================================================================

#[tokio::test]
async fn mpsc_expiry_reports_to_per_sender_channel() {
    let expiry = CollectorChannel::<i32>::new();
    let (tx, rx) = mpsc_channel(4, SHORT_TTL);
    tx.set_expiry_channel(Some(expiry.clone()));
    tx.send(1).unwrap();
    // Wait for expiry + reclaimer.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let err = rx
        .next(Some(Instant::now() + Duration::from_millis(50)))
        .await
        .unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::Timeout);
    // Give reclaimer time.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(expiry.items(), vec![1]);
}

// ===========================================================================
// MPSC: set_expiry_channel / set_shutdown_channel
// ===========================================================================

#[tokio::test]
async fn mpsc_set_shutdown_channel_affects_future_sends() {
    let shutdown_a = CollectorChannel::<i32>::new();
    let shutdown_b = CollectorChannel::<i32>::new();
    let (tx, rx) = mpsc_channel(4, DEFAULT_TTL);
    tx.set_shutdown_channel(Some(shutdown_a.clone()));
    tx.send(1).unwrap(); // captures shutdown_a

    tx.set_shutdown_channel(Some(shutdown_b.clone()));
    tx.send(2).unwrap(); // captures shutdown_b

    tx.shutdown();
    // Item 1 → shutdown_a, item 2 → shutdown_b.
    assert_eq!(shutdown_a.items(), vec![1]);
    assert_eq!(shutdown_b.items(), vec![2]);
    drop(rx);
}

// ===========================================================================
// MPSC: set_channels (atomic update of both)
// ===========================================================================

#[tokio::test]
async fn mpsc_set_channels_updates_both_atomically() {
    let expiry_a = CollectorChannel::<i32>::new();
    let shutdown_a = CollectorChannel::<i32>::new();
    let expiry_b = CollectorChannel::<i32>::new();
    let shutdown_b = CollectorChannel::<i32>::new();
    let (tx, rx) = mpsc_channel(4, DEFAULT_TTL);

    // Set initial channels.
    tx.set_channels(Some(expiry_a.clone()), Some(shutdown_a.clone()));
    tx.send(1).unwrap(); // captures (expiry_a, shutdown_a)

    // Atomically switch both channels.
    tx.set_channels(Some(expiry_b.clone()), Some(shutdown_b.clone()));
    tx.send(2).unwrap(); // captures (expiry_b, shutdown_b)

    tx.shutdown();
    // Item 1 → shutdown_a, item 2 → shutdown_b.
    assert_eq!(shutdown_a.items(), vec![1]);
    assert_eq!(shutdown_b.items(), vec![2]);
    drop(rx);
}

// ===========================================================================
// MPSC: TTL and capacity updates
// ===========================================================================

#[tokio::test]
async fn mpsc_update_ttl_and_capacity() {
    let (tx, _rx) = mpsc_channel(2, DEFAULT_TTL);
    tx.update_ttl(Duration::from_secs(10)).unwrap();
    tx.update_capacity(4);
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    tx.send(3).unwrap();
    tx.send(4).unwrap();
    assert!(tx.send(5).is_err());
}

// ===========================================================================
// InvalidPattern: wrong send variant
// ===========================================================================

// InvalidPattern is enforced at the ring/concurrency layer, already tested
// there. The public API only exposes the correct send method per sender type,
// so InvalidPattern cannot be triggered through the public interface.

// ===========================================================================
// NoRuntime
// ===========================================================================

#[test]
fn spsc_build_without_runtime_returns_no_runtime() {
    let expiry = CollectorChannel::<i32>::new();
    let shutdown = CollectorChannel::<i32>::new();
    let result = SpscBuilder::new(4, DEFAULT_TTL)
        .expiry_channel(expiry)
        .shutdown_channel(shutdown)
        .build();
    let err = result.unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::NoRuntime);
}

#[test]
fn mpsc_build_without_runtime_returns_no_runtime() {
    let result = MpscBuilder::<i32>::new(4, DEFAULT_TTL).build();
    let err = result.unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::NoRuntime);
}

// ===========================================================================
// Concurrent send/receive
// ===========================================================================

#[tokio::test]
async fn concurrent_mpsc_send_receive() {
    let (tx, rx) = mpsc_channel(64, DEFAULT_TTL);
    let count = 100;
    let received = Arc::new(AtomicUsize::new(0));

    // Spawn 4 senders.
    let mut handles = Vec::new();
    for sender_id in 0..4u32 {
        let tx = tx.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..count {
                let val = (sender_id as i32) * 1000 + i;
                while tx.send(val).is_err() {
                    tokio::task::yield_now().await;
                }
            }
        }));
    }

    // Receiver.
    let recv_count = Arc::clone(&received);
    let recv_handle = tokio::spawn(async move {
        loop {
            match rx
                .next(Some(Instant::now() + Duration::from_millis(500)))
                .await
            {
                Ok(_) => {
                    recv_count.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) if e.kind == CaducusErrorKind::Timeout => break,
                Err(e) if e.kind == CaducusErrorKind::Shutdown(()) => break,
                Err(_) => break,
            }
        }
    });

    for h in handles {
        h.await.unwrap();
    }
    // Drop sender to let receiver exit.
    drop(tx);
    recv_handle.await.unwrap();

    assert_eq!(received.load(Ordering::Relaxed), 4 * count as usize);
}

// ===========================================================================
// Receiver wakeup on send
// ===========================================================================

#[tokio::test]
async fn receiver_wakes_on_send() {
    let (tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    let handle = tokio::spawn(async move {
        rx.next(Some(Instant::now() + Duration::from_secs(5)))
            .await
            .unwrap()
    });
    // Small delay then send.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.send(99).unwrap();
    let val = handle.await.unwrap();
    assert_eq!(val, 99);
}

#[tokio::test]
async fn receiver_wakes_on_shutdown() {
    let (tx, rx, _, _) = spsc_channel(4, DEFAULT_TTL);
    let handle =
        tokio::spawn(async move { rx.next(Some(Instant::now() + Duration::from_secs(5))).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.shutdown();
    let result = handle.await.unwrap();
    assert_eq!(result.unwrap_err().kind, CaducusErrorKind::Shutdown(()));
}

// ===========================================================================
// Builder with explicit runtime handle
// ===========================================================================

#[tokio::test]
async fn spsc_build_with_explicit_runtime() {
    let handle = tokio::runtime::Handle::current();
    let expiry = CollectorChannel::<i32>::new();
    let shutdown = CollectorChannel::<i32>::new();
    let (tx, rx) = SpscBuilder::new(4, DEFAULT_TTL)
        .expiry_channel(expiry)
        .shutdown_channel(shutdown)
        .runtime(handle)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 1);
}

#[tokio::test]
async fn mpsc_build_with_explicit_runtime() {
    let handle = tokio::runtime::Handle::current();
    let (tx, rx) = MpscBuilder::<i32>::new(4, DEFAULT_TTL)
        .runtime(handle)
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(100)))
        .await
        .unwrap();
    assert_eq!(val, 1);
}

// ===========================================================================
// TTL validation at build time
// ===========================================================================

#[tokio::test]
async fn spsc_build_rejects_invalid_ttl() {
    let expiry = CollectorChannel::<i32>::new();
    let shutdown = CollectorChannel::<i32>::new();
    let result = SpscBuilder::new(4, Duration::ZERO)
        .expiry_channel(expiry)
        .shutdown_channel(shutdown)
        .build();
    let err = result.unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

#[tokio::test]
async fn mpsc_build_rejects_invalid_ttl() {
    let result = MpscBuilder::<i32>::new(4, Duration::ZERO).build();
    let err = result.unwrap_err();
    assert_eq!(err.kind, CaducusErrorKind::InvalidArgument);
}

// ===========================================================================
// TTL-shrink: end-to-end non-head expiry reporting
// ===========================================================================

#[tokio::test]
async fn spsc_ttl_shrink_reports_non_head_expired_through_channel() {
    // Send A under default TTL, reduce TTL, send B under short TTL. After
    // waiting for B's expiry, the expiry channel should receive B while A
    // remains receivable.
    let expiry = CollectorChannel::new();
    let shutdown = CollectorChannel::new();
    let (tx, rx) = SpscBuilder::new(4, Duration::from_secs(60))
        .expiry_channel(expiry.clone())
        .shutdown_channel(shutdown.clone())
        .build()
        .expect("build should succeed");
    tx.send(1).unwrap();
    tx.update_ttl(Duration::from_millis(20)).unwrap();
    tx.send(2).unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let collected = expiry.items();
    assert!(
        collected.contains(&2),
        "expiry channel should receive non-head expired item 2, got {collected:?}"
    );
    // Item 1 should still be receivable.
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(val, 1);
}

#[tokio::test]
async fn mpsc_ttl_shrink_reports_non_head_expired_through_channel() {
    let (tx, rx, expiry, _shutdown) = mpsc_channel_with_reports(4, Duration::from_secs(60));
    tx.send(1).unwrap();
    tx.update_ttl(Duration::from_millis(20)).unwrap();
    tx.send(2).unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let collected = expiry.items();
    assert!(
        collected.contains(&2),
        "MPSC expiry channel should receive non-head expired item 2, got {collected:?}"
    );
    let val = rx
        .next(Some(Instant::now() + Duration::from_millis(200)))
        .await
        .unwrap();
    assert_eq!(val, 1);
}
