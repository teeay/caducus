// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Weak};
use std::time::Instant;

use tokio::runtime::Handle;
use tokio::sync::Notify;

use crate::concurrency::{ConcurrentRing, DrainMode, PopResult};
use crate::error::{CaducusError, CaducusErrorKind};

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// Reports expired items through their expiry channels.
///
/// Single-attempt delivery. On failure or panic, logs a warning and drops the
/// item. User `ReportChannel` implementations are called inside
/// `catch_unwind` so a panicking channel cannot kill the reclaimer task or
/// unwind through a destructor.
pub(crate) fn report_expired<T: 'static>(items: Vec<PopResult<T>>) {
    for pop in items {
        if let Some(ch) = pop.expiry_channel {
            report_one(ch, pop.item, "expiry");
        }
    }
}

/// Reports shutdown-drained items through their shutdown channels.
///
/// Single-attempt delivery. On failure or panic, logs a warning and drops the
/// item. User `ReportChannel` implementations are called inside
/// `catch_unwind` so a panicking channel cannot unwind through a destructor.
pub(crate) fn report_shutdown<T: 'static>(items: Vec<PopResult<T>>) {
    for pop in items {
        if let Some(ch) = pop.shutdown_channel {
            report_one(ch, pop.item, "shutdown");
        }
    }
}

/// Convenience: shut the ring down and report all drained items.
///
/// Used by sender and receiver `Drop`, and by explicit `shutdown()` calls on
/// senders. The concurrency layer releases the ring mutex inside `shutdown()`
/// before returning the drained items, so reporting always happens outside the
/// lock.
pub(crate) fn shutdown_and_report<T: Send + 'static>(ring: &ConcurrentRing<T>) {
    let items = ring.shutdown();
    drop(ring.reclaimer_reporting_lock());
    report_shutdown(items);
}

/// Delivers a single item to a report channel with unwind isolation.
fn report_one<T: 'static>(ch: Arc<dyn crate::concurrency::ReportChannel<T>>, item: T, label: &str) {
    match catch_unwind(AssertUnwindSafe(|| ch.send(item))) {
        Ok(Ok(())) => {}
        Ok(Err(_item)) => {
            log::warn!("{label} report channel rejected item; dropping");
        }
        Err(_panic) => {
            log::warn!("{label} report channel panicked; dropping item");
        }
    }
}

// ---------------------------------------------------------------------------
// Receiver support
// ---------------------------------------------------------------------------

/// Drains expired items, reports them, and optionally returns the next live
/// item. Called by `Receiver::next` on each iteration of its wait loop.
///
/// Returns `Ok(Some(item))` if a live item was claimed, `Ok(None)` if the
/// buffer is empty (caller should wait), or `Err(Shutdown)` if the buffer is
/// shut down and empty.
pub(crate) fn try_receive<T: Send + 'static>(
    ring: &ConcurrentRing<T>,
) -> Result<Option<T>, CaducusError> {
    let result = ring.drain(Instant::now(), DrainMode::DrainAndClaim);
    report_expired(result.expired);
    match (result.live, result.is_shutdown) {
        (Some(pop), _) => Ok(Some(pop.item)),
        (None, true) => Err(CaducusError {
            kind: CaducusErrorKind::Shutdown(()),
        }),
        (None, false) => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Reclaimer task
// ---------------------------------------------------------------------------

/// Spawns the reclaimer task on the provided runtime.
///
/// The task holds a `Weak` reference to the ring and exits when the weak
/// upgrade fails or the buffer is shut down.
pub(crate) fn spawn_reclaimer<T: Send + 'static>(
    ring: Weak<ConcurrentRing<T>>,
    notify_reclaimer: Arc<Notify>,
    notify_receiver: Arc<Notify>,
    handle: &Handle,
) {
    handle.spawn(reclaimer_loop(ring, notify_reclaimer, notify_receiver));
}

async fn reclaimer_loop<T: 'static>(
    ring: Weak<ConcurrentRing<T>>,
    notify_reclaimer: Arc<Notify>,
    notify_receiver: Arc<Notify>,
) {
    loop {
        // Step 1: upgrade or exit.
        let strong = match ring.upgrade() {
            Some(s) => s,
            None => return,
        };

        // Step 2: create waiter before drain so no notification is lost, prevent
        // concurrent shutdown from draining at the same time
        let waiter = notify_reclaimer.notified();
        tokio::pin!(waiter);
        let report_guard = strong.reclaimer_reporting_lock();

        // Step 3: drain all expired items under one lock.
        let result = strong.drain(Instant::now(), DrainMode::DrainOnly);


        // Step 4: report drained items (outside the ring lock, holding
        // reclaimer_reporting so shutdown_and_report sees the report flushed).
        let had_expired = !result.expired.is_empty();
        {
            report_expired(result.expired);
        }

        // Done reporting, drop report guard
        drop(report_guard);

        // Step 4a: if shutdown, we are done
        if result.is_shutdown {
            return;
        }

        // Drop the strong ref now that reporting is complete.
        drop(strong);

        // Step 4b: if we drained expired items and a live head is now
        // exposed, wake the receiver.
        if had_expired && result.next_deadline.is_some() {
            notify_receiver.notify_waiters();
        }

        // Step 5: sleep until next deadline or notification.
        match result.next_deadline {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    // Deadline already passed (reporting took time). Loop immediately.
                    continue;
                }
                tokio::select! {
                    _ = &mut waiter => {}
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                }
            }
            None => {
                // Buffer empty. Wait for a notification (new send, shutdown, etc.).
                waiter.await;
            }
        }
    }
}
