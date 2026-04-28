// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::runtime::Handle;

use crate::concurrency::{ChannelMode, ConcurrentRing, ReportChannel};
use crate::error::{CaducusError, CaducusErrorKind};
use crate::reclaimer;

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

/// Builder for an SPSC (single-producer, single-consumer) channel.
///
/// Report channels are optional and used for all messages. When a channel is
/// not configured, the corresponding outcomes (expiry or shutdown) are silently
/// dropped.
pub struct SpscBuilder<T: Send + 'static> {
    capacity: usize,
    ttl: Duration,
    expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
    shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    runtime: Option<Handle>,
}

impl<T: Send + 'static> SpscBuilder<T> {
    /// Creates a builder with the given buffer `capacity` and per-item `ttl`.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            expiry_channel: None,
            shutdown_channel: None,
            runtime: None,
        }
    }

    /// Sets the channel that receives items expired due to TTL. Without this,
    /// expired items are dropped silently.
    pub fn expiry_channel(mut self, ch: Arc<dyn ReportChannel<T>>) -> Self {
        self.expiry_channel = Some(ch);
        self
    }

    /// Sets the channel that receives items remaining in the buffer at
    /// shutdown. Without this, shutdown items are dropped silently.
    pub fn shutdown_channel(mut self, ch: Arc<dyn ReportChannel<T>>) -> Self {
        self.shutdown_channel = Some(ch);
        self
    }

    /// Sets an explicit Tokio runtime handle. If not provided, `build()` uses
    /// `Handle::try_current()`.
    pub fn runtime(mut self, handle: Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Builds the channel, returning `(SpscSender<T>, Receiver<T>)`.
    pub fn build(self) -> Result<(SpscSender<T>, super::receiver::Receiver<T>), CaducusError> {
        let handle = resolve_runtime(self.runtime)?;
        let ring = ConcurrentRing::new(
            self.capacity,
            self.ttl,
            ChannelMode::Spsc,
            self.expiry_channel,
            self.shutdown_channel,
        )?;
        let ring = Arc::new(ring);
        let notify_reclaimer = ring.notify_reclaimer_handle();
        let notify_receiver = ring.notify_receiver_handle();
        reclaimer::spawn_reclaimer(
            Arc::downgrade(&ring),
            notify_reclaimer,
            notify_receiver,
            &handle,
        );
        Ok((
            SpscSender {
                ring: Arc::clone(&ring),
            },
            super::receiver::Receiver::new(ring),
        ))
    }
}

/// Builder for an MPSC (multi-producer, single-consumer) channel.
///
/// Report channels are optional and set the initial sender-local defaults.
/// Each item can carry separate reporting channels for each of the producers.
pub struct MpscBuilder<T: Send + 'static> {
    capacity: usize,
    ttl: Duration,
    expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
    shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    runtime: Option<Handle>,
}

impl<T: Send + 'static> MpscBuilder<T> {
    /// Creates a builder with the given buffer `capacity` and per-item `ttl`.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity,
            ttl,
            expiry_channel: None,
            shutdown_channel: None,
            runtime: None,
        }
    }

    /// Sets the initial expiry-report channel inherited by the first
    /// [`MpscSender`]. Subsequent clones snapshot whatever channel was set on
    /// the source sender at clone time.
    pub fn expiry_channel(mut self, ch: Arc<dyn ReportChannel<T>>) -> Self {
        self.expiry_channel = Some(ch);
        self
    }

    /// Sets the initial shutdown-report channel inherited by the first
    /// [`MpscSender`]. See [`MpscBuilder::expiry_channel`] for snapshot
    /// semantics on clone.
    pub fn shutdown_channel(mut self, ch: Arc<dyn ReportChannel<T>>) -> Self {
        self.shutdown_channel = Some(ch);
        self
    }

    /// Sets an explicit Tokio runtime handle. If not provided, `build()` uses
    /// `Handle::try_current()`.
    pub fn runtime(mut self, handle: Handle) -> Self {
        self.runtime = Some(handle);
        self
    }

    /// Builds the channel, returning `(MpscSender<T>, Receiver<T>)`.
    pub fn build(self) -> Result<(MpscSender<T>, super::receiver::Receiver<T>), CaducusError> {
        let handle = resolve_runtime(self.runtime)?;
        let ring = ConcurrentRing::new(self.capacity, self.ttl, ChannelMode::Mpsc, None, None)?;
        let ring = Arc::new(ring);
        let notify_reclaimer = ring.notify_reclaimer_handle();
        let notify_receiver = ring.notify_receiver_handle();
        reclaimer::spawn_reclaimer(
            Arc::downgrade(&ring),
            notify_reclaimer,
            notify_receiver,
            &handle,
        );
        Ok((
            MpscSender {
                ring: Arc::clone(&ring),
                expiry_channel: Mutex::new(self.expiry_channel),
                shutdown_channel: Mutex::new(self.shutdown_channel),
                sender_count: Arc::new(AtomicUsize::new(1)),
            },
            super::receiver::Receiver::new(ring),
        ))
    }
}

fn resolve_runtime(provided: Option<Handle>) -> Result<Handle, CaducusError> {
    match provided {
        Some(h) => Ok(h),
        None => Handle::try_current().map_err(|_| CaducusError {
            kind: CaducusErrorKind::NoRuntime,
        }),
    }
}

// ---------------------------------------------------------------------------
// SpscSender
// ---------------------------------------------------------------------------

/// Single-producer sender. Not cloneable.
///
/// Report channels are fixed at construction inside the ring buffer. Dropping
/// the sender triggers a hard shutdown of the channel: remaining items are
/// drained and reported through the shutdown channel (if configured) and the
/// receiver observes a [`CaducusErrorKind::Shutdown`] on its next call.
pub struct SpscSender<T: Send + 'static> {
    ring: Arc<ConcurrentRing<T>>,
}

impl<T: Send + 'static> std::fmt::Debug for SpscSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpscSender").finish_non_exhaustive()
    }
}

impl<T: Send + 'static> SpscSender<T> {
    /// Sends an item. Fails with [`CaducusErrorKind::Full`] if the buffer is at
    /// capacity or [`CaducusErrorKind::Shutdown`] if the channel has been shut
    /// down. In both cases the rejected item is recoverable via
    /// [`CaducusError::into_inner`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use caducus::SpscBuilder;
    ///
    /// # async fn example() {
    /// let (tx, _rx) = SpscBuilder::<&'static str>::new(8, Duration::from_secs(1))
    ///     .build()
    ///     .unwrap();
    /// tx.send("hello").unwrap();
    /// # }
    /// ```
    pub fn send(&self, item: T) -> Result<(), CaducusError<T>> {
        self.ring.send_spsc(item)
    }

    /// Updates the buffer capacity. Shrinks may evict head items through the
    /// expiry channel; growth allocates lazily.
    pub fn update_capacity(&self, new: usize) {
        self.ring.update_capacity(new);
    }

    /// Updates the per-item TTL. Returns [`CaducusErrorKind::InvalidArgument`]
    /// if the value is outside the supported range.
    pub fn update_ttl(&self, duration: Duration) -> Result<(), CaducusError> {
        self.ring.update_ttl(duration)
    }

    /// Triggers a hard shutdown: drains the buffer through the shutdown
    /// channel and causes future sends to fail with
    /// [`CaducusErrorKind::Shutdown`]. Idempotent.
    pub fn shutdown(&self) {
        reclaimer::shutdown_and_report(&self.ring);
    }

    /// Returns `true` once the channel has been shut down.
    pub fn is_closed(&self) -> bool {
        self.ring.is_shutdown()
    }
}

impl<T: Send + 'static> Drop for SpscSender<T> {
    fn drop(&mut self) {
        reclaimer::shutdown_and_report(&self.ring);
    }
}

// ---------------------------------------------------------------------------
// MpscSender
// ---------------------------------------------------------------------------

/// Multi-producer sender. Cloneable with channel snapshot semantics.
///
/// Each clone captures the current report channels at clone time. Dropping the
/// last clone triggers a hard shutdown of the channel; intermediate drops do
/// not. Per-clone report channels can be reassigned via
/// [`MpscSender::set_expiry_channel`], [`MpscSender::set_shutdown_channel`],
/// or [`MpscSender::set_channels`].
pub struct MpscSender<T: Send + 'static> {
    ring: Arc<ConcurrentRing<T>>,
    expiry_channel: Mutex<Option<Arc<dyn ReportChannel<T>>>>,
    shutdown_channel: Mutex<Option<Arc<dyn ReportChannel<T>>>>,
    sender_count: Arc<AtomicUsize>,
}

impl<T: Send + 'static> std::fmt::Debug for MpscSender<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MpscSender").finish_non_exhaustive()
    }
}

impl<T: Send + 'static> MpscSender<T> {
    /// Sends an item. Fails with [`CaducusErrorKind::Full`] if the buffer is at
    /// capacity or [`CaducusErrorKind::Shutdown`] if the channel has been shut
    /// down. In both cases the rejected item is recoverable via
    /// [`CaducusError::into_inner`]. The item is enqueued with this sender's
    /// current report channels at the moment of the call.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use caducus::MpscBuilder;
    ///
    /// # async fn example() {
    /// let (tx, _rx) = MpscBuilder::<u64>::new(64, Duration::from_secs(2))
    ///     .build()
    ///     .unwrap();
    /// let tx2 = tx.clone();
    /// tx.send(1).unwrap();
    /// tx2.send(2).unwrap();
    /// # }
    /// ```
    pub fn send(&self, item: T) -> Result<(), CaducusError<T>> {
        let exp = self
            .expiry_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let shut = self
            .shutdown_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let expiry = exp.clone();
        let shutdown = shut.clone();
        drop(shut);
        drop(exp);
        self.ring.send_mpsc(item, expiry, shutdown)
    }

    /// Replaces this sender's expiry channel. Affects only items sent after
    /// the call returns; in-flight items keep the channel they were enqueued
    /// with.
    pub fn set_expiry_channel(&self, ch: Option<Arc<dyn ReportChannel<T>>>) {
        *self
            .expiry_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ch;
    }

    /// Replaces this sender's shutdown channel. See
    /// [`MpscSender::set_expiry_channel`] for ordering semantics.
    pub fn set_shutdown_channel(&self, ch: Option<Arc<dyn ReportChannel<T>>>) {
        *self
            .shutdown_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ch;
    }

    /// Atomically sets both report channels. Prevents a concurrent `send`
    /// from observing one old and one new channel when both need to change
    /// together.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::sync::Arc;
    /// use std::time::Duration;
    /// use caducus::{MpscBuilder, ReportChannel};
    ///
    /// struct Discard;
    /// impl<T: Send + 'static> ReportChannel<T> for Discard {
    ///     fn send(&self, _: T) -> Result<(), T> { Ok(()) }
    /// }
    ///
    /// # async fn example() {
    /// let (tx, _rx) = MpscBuilder::<i32>::new(16, Duration::from_secs(1))
    ///     .build()
    ///     .unwrap();
    /// tx.set_channels(Some(Arc::new(Discard)), Some(Arc::new(Discard)));
    /// # }
    /// ```
    pub fn set_channels(
        &self,
        expiry: Option<Arc<dyn ReportChannel<T>>>,
        shutdown: Option<Arc<dyn ReportChannel<T>>>,
    ) {
        let mut exp = self
            .expiry_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut shut = self
            .shutdown_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *exp = expiry;
        *shut = shutdown;
    }

    /// Updates the buffer capacity. Shrinks may evict head items through their
    /// expiry channels; growth allocates lazily.
    pub fn update_capacity(&self, new: usize) {
        self.ring.update_capacity(new);
    }

    /// Updates the per-item TTL. Returns [`CaducusErrorKind::InvalidArgument`]
    /// if the value is outside the supported range.
    pub fn update_ttl(&self, duration: Duration) -> Result<(), CaducusError> {
        self.ring.update_ttl(duration)
    }

    /// Triggers a hard shutdown: drains the buffer through each item's
    /// shutdown channel and causes future sends to fail with
    /// [`CaducusErrorKind::Shutdown`]. Idempotent.
    pub fn shutdown(&self) {
        reclaimer::shutdown_and_report(&self.ring);
    }

    /// Returns `true` once the channel has been shut down.
    pub fn is_closed(&self) -> bool {
        self.ring.is_shutdown()
    }
}

impl<T: Send + 'static> Clone for MpscSender<T> {
    fn clone(&self) -> Self {
        self.sender_count.fetch_add(1, Ordering::Relaxed);
        let exp = self
            .expiry_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let shut = self
            .shutdown_channel
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let expiry = exp.clone();
        let shutdown = shut.clone();
        drop(shut);
        drop(exp);
        Self {
            ring: Arc::clone(&self.ring),
            expiry_channel: Mutex::new(expiry),
            shutdown_channel: Mutex::new(shutdown),
            sender_count: Arc::clone(&self.sender_count),
        }
    }
}

impl<T: Send + 'static> Drop for MpscSender<T> {
    fn drop(&mut self) {
        // Arc::drop ordering pattern: Release on all decrements, Acquire
        // fence on the one that sees zero.
        if self.sender_count.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            reclaimer::shutdown_and_report(&self.ring);
        }
    }
}
