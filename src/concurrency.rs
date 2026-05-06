// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

#[path = "concurrency/ring_buffer.rs"]
mod ring_buffer;

// Re-export types that upper layers (sender, receiver, reclaimer) need.
pub use ring_buffer::ReportChannel;
pub(crate) use ring_buffer::{ChannelMode, PopResult};

use crate::error::{CaducusError, CaducusErrorKind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ring_buffer::Ring;

pub(crate) fn expires_at_from_ttl(ttl: Duration) -> Result<Instant, ()> {
    Ring::<()>::expires_at_from_ttl(ttl)
}

pub(crate) fn validate_deadline(deadline: Instant) -> Result<Instant, ()> {
    Ring::<()>::validate_deadline(deadline)
}

// ---------------------------------------------------------------------------
// Drain types
// ---------------------------------------------------------------------------

// Controls whether `drain` also claims the next live item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DrainMode {
    // Drain expired heads only. Used by the reclaimer task.
    DrainOnly,
    // Drain expired heads and pop the next live item. Used by the receiver
    // path through the reclaimer layer.
    DrainAndClaim,
}

// Result of a single `drain` call.
pub(crate) struct DrainResult<T> {
    // Expired items drained from the head of the buffer.
    pub expired: Vec<PopResult<T>>,
    // The next live item, if `DrainAndClaim` was requested and one was
    // available.
    pub live: Option<PopResult<T>>,
    // Expiry deadline of the current head after the drain, or `None` if
    // the buffer is empty.
    pub next_deadline: Option<Instant>,
    // Whether the buffer has been shut down.
    pub is_shutdown: bool,
}

// ---------------------------------------------------------------------------
// ConcurrentRing
// ---------------------------------------------------------------------------

// Concurrency wrapper around `Ring<T>`.
pub(crate) struct ConcurrentRing<T> {
    ring: Mutex<Ring<T>>,
    reclaimer_reporting: Mutex<()>,
    notify_reclaimer: Arc<tokio::sync::Notify>,
    notify_receiver: Arc<tokio::sync::Notify>,
}

impl<T> ConcurrentRing<T> {
    // Creates a new concurrency-protected ring buffer.
    pub fn new(
        capacity: usize,
        ttl: Duration,
        mode: ChannelMode,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<Self, CaducusError> {
        Ok(Self {
            ring: Mutex::new(Ring::new(
                capacity,
                ttl,
                mode,
                expiry_channel,
                shutdown_channel,
            )?),
            reclaimer_reporting: Mutex::new(()),
            notify_reclaimer: Arc::new(tokio::sync::Notify::new()),
            notify_receiver: Arc::new(tokio::sync::Notify::new()),
        })
    }

    // Held by the reclaimer task while reporting a drained batch; acquired and
    // released as a barrier by `shutdown_and_report`.
    pub fn reclaimer_reporting_lock(&self) -> MutexGuard<'_, ()> {
        self.reclaimer_reporting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    // Returns a clone of the reclaimer `Notify` handle.
    pub fn notify_reclaimer_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify_reclaimer)
    }

    // Returns a clone of the receiver `Notify` handle.
    pub fn notify_receiver_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.notify_receiver)
    }

    // -----------------------------------------------------------------------
    // Send path
    // -----------------------------------------------------------------------

    // SPSC send. Delegates to `ring.try_push_spsc`.
    pub fn send_spsc(&self, item: T) -> Result<(), CaducusError<T>> {
        let mut ring = self.lock();
        ring.try_push_spsc(item)?;
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
        Ok(())
    }

    // SPSC send using a caller-validated absolute expiry deadline.
    pub(crate) fn send_spsc_with_expires_at(
        &self,
        item: T,
        expires_at: Instant,
    ) -> Result<(), CaducusError<T>> {
        let mut ring = self.lock();
        ring.try_push_spsc_with_expires_at(item, expires_at)?;
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
        Ok(())
    }

    // MPSC send. Delegates to `ring.try_push_mpsc`.
    pub fn send_mpsc(
        &self,
        item: T,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<(), CaducusError<T>> {
        let mut ring = self.lock();
        ring.try_push_mpsc(item, expiry_channel, shutdown_channel)?;
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
        Ok(())
    }

    // MPSC send using a caller-validated absolute expiry deadline.
    pub(crate) fn send_mpsc_with_expires_at(
        &self,
        item: T,
        expires_at: Instant,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<(), CaducusError<T>> {
        let mut ring = self.lock();
        ring.try_push_mpsc_with_expires_at(item, expires_at, expiry_channel, shutdown_channel)?;
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Drain path
    // -----------------------------------------------------------------------

    // Unified drain: drains all expired heads and optionally claims the next
    // live item.
    pub fn drain(&self, now: Instant, mode: DrainMode) -> DrainResult<T> {
        let mut ring = self.lock();
        let expired = ring.drain_expired(now);
        let live = match mode {
            DrainMode::DrainAndClaim => ring.try_pop(),
            DrainMode::DrainOnly => None,
        };
        let next_deadline = ring.peek_expires_at();
        let is_shutdown = ring.is_shutdown();
        drop(ring);
        DrainResult {
            expired,
            live,
            next_deadline,
            is_shutdown,
        }
    }

    // -----------------------------------------------------------------------
    // Configuration
    // -----------------------------------------------------------------------

    // Updates the TTL.
    pub fn update_ttl(&self, duration: Duration) -> Result<(), CaducusError> {
        let mut ring = self.lock();
        ring.set_ttl(duration)?;
        Ok(())
    }

    // Updates the buffer capacity.
    pub fn update_capacity(&self, new: usize) {
        let mut ring = self.lock();
        ring.request_capacity(new);
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    // Shuts down the buffer and returns all drained items.
    pub fn shutdown(&self) -> Vec<PopResult<T>> {
        let mut ring = self.lock();
        let items = ring.shutdown();
        drop(ring);
        self.notify_reclaimer.notify_waiters();
        self.notify_receiver.notify_waiters();
        items
    }

    // Returns whether the buffer has been shut down.
    pub fn is_shutdown(&self) -> bool {
        let ring = self.lock();
        ring.is_shutdown()
    }

    // -----------------------------------------------------------------------
    // Mutex helper
    // -----------------------------------------------------------------------

    // Locks the mutex, recovering from poisoning.
    fn lock(&self) -> MutexGuard<'_, Ring<T>> {
        self.ring.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
