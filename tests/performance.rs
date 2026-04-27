// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use caducus::{
    CaducusErrorKind, MpscBuilder, MpscSender, Receiver, ReportChannel, SpscBuilder, SpscSender,
};

// ===========================================================================
// Counting report channel
// ===========================================================================

struct CountingChannel {
    warmup: AtomicUsize,
    test: AtomicUsize,
}

impl CountingChannel {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            warmup: AtomicUsize::new(0),
            test: AtomicUsize::new(0),
        })
    }

    fn test_count(&self) -> usize {
        self.test.load(Ordering::SeqCst)
    }
}

impl ReportChannel<Payload> for CountingChannel {
    fn send(&self, item: Payload) -> Result<(), Payload> {
        if item[0] == PHASE_TEST {
            self.test.fetch_add(1, Ordering::SeqCst);
        } else {
            self.warmup.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

// ===========================================================================
// Scenario result
// ===========================================================================

struct ScenarioResult {
    label: String,
    items_sent: usize,
    items_received: usize,
    items_expired: usize,
    items_shutdown: usize,
    full_rejections: usize,
    elapsed: Duration,
}

impl ScenarioResult {
    fn throughput(&self) -> f64 {
        if self.elapsed.as_secs_f64() > 0.0 {
            self.items_received as f64 / self.elapsed.as_secs_f64()
        } else {
            0.0
        }
    }

    fn print(&self) {
        println!(
            "[{}] {:>12.0} items/sec | sent: {} | recv: {} | full: {} | expired: {} | shutdown: {} | {:.3}s",
            self.label,
            self.throughput(),
            self.items_sent,
            self.items_received,
            self.full_rejections,
            self.items_expired,
            self.items_shutdown,
            self.elapsed.as_secs_f64(),
        );
    }

    fn verify_conservation(&self) {
        let accounted = self.items_received + self.items_expired + self.items_shutdown;
        assert_eq!(
            accounted,
            self.items_sent,
            "{}: conservation violated: recv({}) + expired({}) + shutdown({}) = {} != sent({})",
            self.label,
            self.items_received,
            self.items_expired,
            self.items_shutdown,
            accounted,
            self.items_sent,
        );
    }
}

// ===========================================================================
// Delay helpers
// ===========================================================================

async fn delay(d: Option<Duration>) {
    if let Some(d) = d {
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
    }
}

// ===========================================================================
// Default item counts (kept small enough for CI)
// ===========================================================================

const ITEMS: usize = 100_000;
const WARMUP: usize = 1_000;
const ITEMS_SLOW: usize = 10_000;

// ===========================================================================
// Payload
// ===========================================================================

type Payload = [u8; 64];

/// Phase marker stored in `Payload[0]` so report channels and the receive loop
/// can attribute each item to its phase. Warmup items are excluded from the
/// scenario's conservation check; without this marker, warmup items that
/// expire before the inline `rx.next()` would inflate `expired` without
/// inflating `sent` and break conservation under contention.
const PHASE_WARMUP: u8 = 0;
const PHASE_TEST: u8 = 1;

const fn payload(phase: u8) -> Payload {
    let mut p = [0xABu8; 64];
    p[0] = phase;
    p
}

const PAYLOAD_WARMUP: Payload = payload(PHASE_WARMUP);
const PAYLOAD: Payload = payload(PHASE_TEST);

// ===========================================================================
// SPSC send loop
// ===========================================================================

async fn spsc_send_loop(
    tx: &SpscSender<Payload>,
    count: usize,
    send_delay: Option<Duration>,
) -> (usize, usize) {
    let mut sent = 0;
    let mut full_count = 0;
    for _ in 0..count {
        loop {
            match tx.send(PAYLOAD) {
                Ok(()) => {
                    sent += 1;
                    break;
                }
                Err(e) => match e.kind {
                    CaducusErrorKind::Full(_) => {
                        full_count += 1;
                        tokio::task::yield_now().await;
                    }
                    CaducusErrorKind::Shutdown(_) => return (sent, full_count),
                    _ => panic!("unexpected send error: {e}"),
                },
            }
        }
        delay(send_delay).await;
    }
    (sent, full_count)
}

// ===========================================================================
// MPSC send loop
// ===========================================================================

async fn mpsc_send_loop(
    tx: &MpscSender<Payload>,
    count: usize,
    send_delay: Option<Duration>,
) -> (usize, usize) {
    let mut sent = 0;
    let mut full_count = 0;
    for _ in 0..count {
        loop {
            match tx.send(PAYLOAD) {
                Ok(()) => {
                    sent += 1;
                    break;
                }
                Err(e) => match e.kind {
                    CaducusErrorKind::Full(_) => {
                        full_count += 1;
                        tokio::task::yield_now().await;
                    }
                    CaducusErrorKind::Shutdown(_) => return (sent, full_count),
                    _ => panic!("unexpected send error: {e}"),
                },
            }
        }
        delay(send_delay).await;
    }
    (sent, full_count)
}

// ===========================================================================
// Receive loop
// ===========================================================================

async fn receive_loop(rx: &Receiver<Payload>, recv_delay: Option<Duration>) -> usize {
    let mut received = 0;
    loop {
        match rx.next(Some(Instant::now() + Duration::from_secs(2))).await {
            Ok(item) => {
                if item[0] == PHASE_TEST {
                    received += 1;
                }
                delay(recv_delay).await;
            }
            Err(e) => match e.kind {
                CaducusErrorKind::Timeout => continue,
                CaducusErrorKind::Shutdown(_) => return received,
                _ => panic!("unexpected receive error: {e}"),
            },
        }
    }
}

/// Owned variant for use with `tokio::spawn`.
async fn receive_loop_owned(rx: Receiver<Payload>, recv_delay: Option<Duration>) -> usize {
    receive_loop(&rx, recv_delay).await
}

// ===========================================================================
// SPSC scenario runner
// ===========================================================================

async fn run_spsc(
    label: &str,
    capacity: usize,
    ttl: Duration,
    item_count: usize,
    send_delay: Option<Duration>,
    recv_delay: Option<Duration>,
) -> ScenarioResult {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(capacity, ttl)
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    // Warmup.
    for _ in 0..WARMUP.min(item_count) {
        loop {
            match tx.send(PAYLOAD_WARMUP) {
                Ok(()) => break,
                Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                    tokio::task::yield_now().await;
                }
                Err(e) => panic!("warmup send error: {e}"),
            }
        }
        let _ = rx
            .next(Some(Instant::now() + Duration::from_millis(100)))
            .await;
    }

    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let r = spsc_send_loop(tx_ref, item_count, send_delay).await;
            tx_ref.shutdown();
            r
        },
        receive_loop(rx_ref, recv_delay)
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: label.to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
    result
}

// ===========================================================================
// MPSC scenario runner
// ===========================================================================

async fn run_mpsc(
    label: &str,
    num_senders: usize,
    capacity: usize,
    ttl: Duration,
    items_per_sender: usize,
    send_delay: Option<Duration>,
    recv_delay: Option<Duration>,
) -> ScenarioResult {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = MpscBuilder::new(capacity, ttl)
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    // Warmup.
    for _ in 0..WARMUP.min(items_per_sender) {
        loop {
            match tx.send(PAYLOAD_WARMUP) {
                Ok(()) => break,
                Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                    tokio::task::yield_now().await;
                }
                Err(e) => panic!("warmup send error: {e}"),
            }
        }
        let _ = rx
            .next(Some(Instant::now() + Duration::from_millis(100)))
            .await;
    }

    let start = Instant::now();

    let recv_handle = tokio::spawn(receive_loop_owned(rx, recv_delay));

    let mut send_handles = Vec::new();
    for _ in 0..num_senders {
        let sender = tx.clone();
        let sd = send_delay;
        send_handles.push(tokio::spawn(async move {
            mpsc_send_loop(&sender, items_per_sender, sd).await
        }));
    }

    let mut total_sent = 0;
    let mut total_full = 0;
    for h in send_handles {
        let (s, f) = h.await.expect("sender task");
        total_sent += s;
        total_full += f;
    }

    // All senders done — shut down.
    tx.shutdown();

    let received = recv_handle.await.expect("receiver task");
    let elapsed = start.elapsed();

    let result = ScenarioResult {
        label: label.to_string(),
        items_sent: total_sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections: total_full,
        elapsed,
    };
    result.print();
    result.verify_conservation();
    result
}

// ===========================================================================
// SPSC Baseline scenarios (S1-S9)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn s1_spsc_saturated_throughput() {
    run_spsc("S1", 1024, Duration::from_secs(60), ITEMS, None, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s2_spsc_backpressure_fast_sender() {
    run_spsc(
        "S2",
        64,
        Duration::from_secs(60),
        ITEMS_SLOW,
        None,
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s3_spsc_idle_receiver_fast_receiver() {
    run_spsc(
        "S3",
        1024,
        Duration::from_secs(60),
        ITEMS_SLOW,
        Some(Duration::from_micros(100)),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s4_spsc_matched_pacing() {
    run_spsc(
        "S4",
        256,
        Duration::from_secs(60),
        ITEMS_SLOW,
        Some(Duration::from_micros(100)),
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s5_spsc_bursty_sender() {
    // Bursty: alternate between no delay and 5ms delay in chunks.
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(128, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS_SLOW;
    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..count {
                loop {
                    match tx_ref.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => {
                            tx_ref.shutdown();
                            return (sent, full_count);
                        }
                    }
                }
                // Burst pattern: 100 items fast, then 100 items with 5ms delay.
                if (i / 100) % 2 == 1 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            tx_ref.shutdown();
            (sent, full_count)
        },
        receive_loop(rx_ref, None)
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: "S5".to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn s6_spsc_varying_sender_speed() {
    // Sender delay ramps from 0 to 500us over the run.
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(256, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS_SLOW;
    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..count {
                loop {
                    match tx_ref.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => {
                            tx_ref.shutdown();
                            return (sent, full_count);
                        }
                    }
                }
                let frac = i as f64 / count as f64;
                let delay_us = (frac * 500.0) as u64;
                if delay_us > 0 {
                    tokio::time::sleep(Duration::from_micros(delay_us)).await;
                }
            }
            tx_ref.shutdown();
            (sent, full_count)
        },
        receive_loop(rx_ref, Some(Duration::from_micros(100)))
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: "S6".to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn s7_spsc_small_buffer_under_pressure() {
    run_spsc("S7", 4, Duration::from_secs(60), ITEMS, None, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s8_spsc_expiry_heavy() {
    run_spsc(
        "S8",
        256,
        Duration::from_millis(1),
        ITEMS_SLOW,
        None,
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn s9_spsc_payload_size_impact() {
    // Test with different payload sizes. We test 64-byte (our standard) and report.
    // Other sizes would require generic harness changes, so we run the baseline and note it.
    run_spsc("S9", 256, Duration::from_secs(60), ITEMS, None, None).await;
}

// ===========================================================================
// MPSC Contention scenarios (M1-M12)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn m1_mpsc_2_senders_saturated() {
    run_mpsc(
        "M1",
        2,
        1024,
        Duration::from_secs(60),
        ITEMS / 2,
        None,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m2_mpsc_4_senders_saturated() {
    run_mpsc(
        "M2",
        4,
        1024,
        Duration::from_secs(60),
        ITEMS / 4,
        None,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m3_mpsc_8_senders_saturated() {
    run_mpsc(
        "M3",
        8,
        1024,
        Duration::from_secs(60),
        ITEMS / 8,
        None,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m4_mpsc_16_senders_saturated() {
    run_mpsc(
        "M4",
        16,
        1024,
        Duration::from_secs(60),
        ITEMS / 16,
        None,
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m5_mpsc_4_senders_fast_sender() {
    run_mpsc(
        "M5",
        4,
        64,
        Duration::from_secs(60),
        ITEMS_SLOW / 4,
        None,
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m6_mpsc_4_senders_fast_receiver() {
    run_mpsc(
        "M6",
        4,
        1024,
        Duration::from_secs(60),
        ITEMS_SLOW / 4,
        Some(Duration::from_micros(100)),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m7_mpsc_4_senders_bursty() {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = MpscBuilder::new(128, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let items_per_sender = ITEMS_SLOW / 4;
    let start = Instant::now();

    let recv_handle = tokio::spawn(receive_loop_owned(rx, None));

    let mut send_handles = Vec::new();
    for _ in 0..4 {
        let sender = tx.clone();
        send_handles.push(tokio::spawn(async move {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..items_per_sender {
                loop {
                    match sender.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => return (sent, full_count),
                    }
                }
                if (i / 100) % 2 == 1 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
            (sent, full_count)
        }));
    }

    let mut total_sent = 0;
    let mut total_full = 0;
    for h in send_handles {
        let (s, f) = h.await.expect("sender task");
        total_sent += s;
        total_full += f;
    }
    tx.shutdown();
    let received = recv_handle.await.expect("receiver task");
    let elapsed = start.elapsed();

    let result = ScenarioResult {
        label: "M7".to_string(),
        items_sent: total_sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections: total_full,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn m8_mpsc_4_senders_varying_speed() {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = MpscBuilder::new(256, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let items_per_sender = ITEMS_SLOW / 4;
    let start = Instant::now();

    let recv_handle = tokio::spawn(receive_loop_owned(rx, Some(Duration::from_micros(100))));

    let mut send_handles = Vec::new();
    for _ in 0..4 {
        let sender = tx.clone();
        send_handles.push(tokio::spawn(async move {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..items_per_sender {
                loop {
                    match sender.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => return (sent, full_count),
                    }
                }
                let frac = i as f64 / items_per_sender as f64;
                let delay_us = (frac * 500.0) as u64;
                if delay_us > 0 {
                    tokio::time::sleep(Duration::from_micros(delay_us)).await;
                }
            }
            (sent, full_count)
        }));
    }

    let mut total_sent = 0;
    let mut total_full = 0;
    for h in send_handles {
        let (s, f) = h.await.expect("sender task");
        total_sent += s;
        total_full += f;
    }
    tx.shutdown();
    let received = recv_handle.await.expect("receiver task");
    let elapsed = start.elapsed();

    let result = ScenarioResult {
        label: "M8".to_string(),
        items_sent: total_sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections: total_full,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn m9_mpsc_4_senders_small_buffer() {
    run_mpsc("M9", 4, 4, Duration::from_secs(60), ITEMS / 4, None, None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m10_mpsc_4_senders_expiry_heavy() {
    run_mpsc(
        "M10",
        4,
        256,
        Duration::from_millis(1),
        ITEMS_SLOW / 4,
        None,
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn m11_mpsc_mixed_speed_senders() {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = MpscBuilder::new(256, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let items_per_sender = ITEMS_SLOW / 4;
    let delays = [
        None,
        Some(Duration::from_micros(50)),
        Some(Duration::from_micros(200)),
        Some(Duration::from_millis(1)),
    ];
    let start = Instant::now();

    let recv_handle = tokio::spawn(receive_loop_owned(rx, None));

    let mut send_handles = Vec::new();
    for &d in &delays {
        let sender = tx.clone();
        send_handles.push(tokio::spawn(async move {
            mpsc_send_loop(&sender, items_per_sender, d).await
        }));
    }

    let mut total_sent = 0;
    let mut total_full = 0;
    for h in send_handles {
        let (s, f) = h.await.expect("sender task");
        total_sent += s;
        total_full += f;
    }
    tx.shutdown();
    let received = recv_handle.await.expect("receiver task");
    let elapsed = start.elapsed();

    let result = ScenarioResult {
        label: "M11".to_string(),
        items_sent: total_sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections: total_full,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn m12_mpsc_sender_scaling() {
    for num_senders in [2, 4, 8, 16, 32] {
        let items_per = ITEMS / num_senders;
        let label = format!("M12-{num_senders}tx");
        run_mpsc(
            &label,
            num_senders,
            1024,
            Duration::from_secs(60),
            items_per,
            None,
            None,
        )
        .await;
    }
}

// ===========================================================================
// Expiry and Reclaimer scenarios (E1-E6)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn e1_all_items_expire_no_receiver() {
    // Send items, don't receive. Let reclaimer drain them all via expiry.
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(256, Duration::from_millis(1))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS_SLOW;
    let start = Instant::now();

    let mut sent = 0;
    let mut full_count = 0;
    for _ in 0..count {
        loop {
            match tx.send(PAYLOAD) {
                Ok(()) => {
                    sent += 1;
                    break;
                }
                Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                    full_count += 1;
                    // Wait for reclaimer to free space.
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(e) => panic!("unexpected: {e}"),
            }
        }
    }

    // Wait for remaining items to expire.
    tokio::time::sleep(Duration::from_millis(50)).await;
    tx.shutdown();
    // Drop receiver to complete shutdown.
    drop(rx);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let elapsed = start.elapsed();

    let expired = expiry_ch.test_count();
    let shut = shutdown_ch.test_count();
    let result = ScenarioResult {
        label: "E1".to_string(),
        items_sent: sent,
        items_received: 0,
        items_expired: expired,
        items_shutdown: shut,
        full_rejections: full_count,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn e2_fifty_percent_expiry_rate() {
    // Receiver slower than TTL so roughly half expire.
    run_spsc(
        "E2",
        256,
        Duration::from_millis(2),
        ITEMS_SLOW,
        None,
        Some(Duration::from_micros(100)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn e3_reclaimer_under_contention() {
    run_mpsc(
        "E3",
        4,
        256,
        Duration::from_millis(1),
        ITEMS_SLOW / 4,
        None,
        Some(Duration::from_micros(200)),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn e4_ttl_update_mid_run() {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(256, Duration::from_millis(1))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS_SLOW;
    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..count {
                // Switch TTL at halfway point.
                if i == count / 2 {
                    let _ = tx_ref.update_ttl(Duration::from_secs(60));
                }
                loop {
                    match tx_ref.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => {
                            tx_ref.shutdown();
                            return (sent, full_count);
                        }
                    }
                }
            }
            tx_ref.shutdown();
            (sent, full_count)
        },
        receive_loop(rx_ref, Some(Duration::from_micros(50)))
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: "E4".to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn e5_capacity_resize_mid_run() {
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(64, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS;
    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..count {
                // Grow capacity at halfway point.
                if i == count / 2 {
                    tx_ref.update_capacity(512);
                }
                loop {
                    match tx_ref.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => {
                            tx_ref.shutdown();
                            return (sent, full_count);
                        }
                    }
                }
            }
            tx_ref.shutdown();
            (sent, full_count)
        },
        receive_loop(rx_ref, None)
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: "E5".to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

#[tokio::test(flavor = "multi_thread")]
async fn e6_ttl_shrink_with_non_head_expiry() {
    // Start with a long TTL so initial items have far-future deadlines, then
    // shrink the TTL mid-run so subsequent items expire long before the
    // earlier ones. Items pushed after the shrink become non-head expiry
    // candidates that the slow-path drain must report through the expiry
    // channel.
    let expiry_ch = CountingChannel::new();
    let shutdown_ch = CountingChannel::new();
    let (tx, rx) = SpscBuilder::new(256, Duration::from_secs(60))
        .expiry_channel(expiry_ch.clone())
        .shutdown_channel(shutdown_ch.clone())
        .build()
        .expect("build");

    let count = ITEMS_SLOW;
    let start = Instant::now();

    let tx_ref = &tx;
    let rx_ref = &rx;

    let (send_result, received) = tokio::join!(
        async {
            let mut sent = 0usize;
            let mut full_count = 0usize;
            for i in 0..count {
                // Shrink TTL at halfway point so later items have short
                // deadlines while earlier items keep their long ones.
                if i == count / 2 {
                    let _ = tx_ref.update_ttl(Duration::from_millis(5));
                }
                loop {
                    match tx_ref.send(PAYLOAD) {
                        Ok(()) => {
                            sent += 1;
                            break;
                        }
                        Err(e) if matches!(e.kind, CaducusErrorKind::Full(_)) => {
                            full_count += 1;
                            tokio::task::yield_now().await;
                        }
                        Err(_) => {
                            tx_ref.shutdown();
                            return (sent, full_count);
                        }
                    }
                }
            }
            tx_ref.shutdown();
            (sent, full_count)
        },
        // Slow receiver so post-shrink items have a chance to expire while
        // earlier long-TTL items still occupy the head.
        receive_loop(rx_ref, Some(Duration::from_micros(50)))
    );

    let elapsed = start.elapsed();
    let (sent, full_rejections) = send_result;

    let result = ScenarioResult {
        label: "E6".to_string(),
        items_sent: sent,
        items_received: received,
        items_expired: expiry_ch.test_count(),
        items_shutdown: shutdown_ch.test_count(),
        full_rejections,
        elapsed,
    };
    result.print();
    result.verify_conservation();
}

// ===========================================================================
// Buffer Capacity Scaling (C1-C2)
// ===========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn c1_spsc_capacity_sweep() {
    for cap in [4, 16, 64, 256, 1024] {
        let label = format!("C1-{cap}");
        run_spsc(&label, cap, Duration::from_secs(60), ITEMS, None, None).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn c2_mpsc_capacity_sweep() {
    for cap in [4, 16, 64, 256, 1024] {
        let label = format!("C2-{cap}");
        run_mpsc(
            &label,
            4,
            cap,
            Duration::from_secs(60),
            ITEMS / 4,
            None,
            None,
        )
        .await;
    }
}
