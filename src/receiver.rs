// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::concurrency::ConcurrentRing;
use crate::error::{CaducusError, CaducusErrorKind};
use crate::reclaimer;

/// Default receive timeout when `None` is passed to `next`.
const DEFAULT_RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);

/// Single-consumer receiver. Not cloneable. Mode-agnostic.
///
/// `next` returns the owned `T`, never internal types.
pub struct Receiver<T: Send + 'static> {
    ring: Arc<ConcurrentRing<T>>,
    notify: Arc<tokio::sync::Notify>,
}

impl<T: Send + 'static> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver").finish_non_exhaustive()
    }
}

impl<T: Send + 'static> Receiver<T> {
    pub(crate) fn new(ring: Arc<ConcurrentRing<T>>) -> Self {
        let notify = ring.notify_receiver_handle();
        Self { ring, notify }
    }

    /// Waits for the next live item, with an absolute deadline.
    ///
    /// Passing `None` uses the default 1-second timeout; it does not wait
    /// indefinitely. Any expired items encountered are reported through
    /// their expiry channels immediately on each iteration. Returns
    /// [`CaducusErrorKind::Timeout`] if the deadline elapses without an item,
    /// or [`CaducusErrorKind::Shutdown`] if the channel has been shut down.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::{Duration, Instant};
    /// use caducus::SpscBuilder;
    ///
    /// # async fn example() {
    /// let (tx, rx) = SpscBuilder::<u32>::new(8, Duration::from_secs(1))
    ///     .build()
    ///     .unwrap();
    /// tx.send(7).unwrap();
    ///
    /// let item = rx.next(Some(Instant::now() + Duration::from_millis(100))).await.unwrap();
    /// assert_eq!(item, 7);
    /// # }
    /// ```
    pub async fn next(&self, deadline: Option<Instant>) -> Result<T, CaducusError> {
        let deadline = deadline.unwrap_or_else(|| Instant::now() + DEFAULT_RECEIVE_TIMEOUT);

        loop {
            if let Some(item) = reclaimer::try_receive(&self.ring)? {
                return Ok(item);
            }

            // Double-check Notify pattern: create waiter before recheck.
            let waiter = self.notify.notified();
            tokio::pin!(waiter);

            if let Some(item) = reclaimer::try_receive(&self.ring)? {
                return Ok(item);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(CaducusError {
                    kind: CaducusErrorKind::Timeout,
                });
            }
            tokio::select! {
                _ = &mut waiter => {}
                _ = tokio::time::sleep(remaining) => {
                    return Err(CaducusError { kind: CaducusErrorKind::Timeout });
                }
            }
        }
    }

    /// Returns `true` when the channel has been shut down.
    pub fn is_closed(&self) -> bool {
        self.ring.is_shutdown()
    }
}

impl<T: Send + 'static> Drop for Receiver<T> {
    fn drop(&mut self) {
        reclaimer::shutdown_and_report(&self.ring);
    }
}
