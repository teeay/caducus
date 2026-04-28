// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use super::{CaducusError, CaducusErrorKind};

/// Process-wide sentinel `Instant` used as the placeholder expiry deadline for
/// empty slots. Lazily initialised on the first ring construction in the
/// process. Because `Instant::now()` at first init always precedes any later
/// `now`, the sentinel sorts as "in the past" for all later expiry comparisons.
/// It is never read on occupied slots and never compared as a real deadline.
static SLOT_SENTINEL: OnceLock<Instant> = OnceLock::new();

fn slot_sentinel() -> Instant {
    *SLOT_SENTINEL.get_or_init(Instant::now)
}

/// Report channel trait for expiry and shutdown reporting.
///
/// Implementations are called by the reclaimer to surface items that expired
/// or remained in the buffer at shutdown. The implementation must not panic;
/// the reclaimer invokes `send` under unwind isolation, but a panicking
/// implementation will still drop the item.
///
/// Defined here so storage can hold handles without depending on sender
/// internals. The storage layer stores these handles but never calls them.
pub trait ReportChannel<T>: Send + Sync + 'static {
    /// Hands the item to the report sink. Returns the item back as `Err` if
    /// the sink cannot accept it (e.g. closed).
    fn send(&self, item: T) -> Result<(), T>;
}

/// Operating mode, set at construction and immutable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelMode {
    Spsc,
    Mpsc,
}

/// Result of popping or draining an item from the ring buffer.
///
/// Carries the extracted payload and all associated metadata so the caller
/// can make reporting decisions after the item has left storage.
pub(crate) struct PopResult<T> {
    pub item: T,
    pub expires_at: Instant,
    pub expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
    pub shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
}

impl<T: std::fmt::Debug> std::fmt::Debug for PopResult<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PopResult")
            .field("item", &self.item)
            .field("expires_at", &self.expires_at)
            .field(
                "expiry_channel",
                &self.expiry_channel.as_ref().map(|_| ".."),
            )
            .field(
                "shutdown_channel",
                &self.shutdown_channel.as_ref().map(|_| ".."),
            )
            .finish()
    }
}

/// A single slot in the ring buffer.
///
/// The slot structure is identical in both modes for predictable memory use.
/// Only the payload (`item`) uses move semantics via `Option<T>`.
struct Slot<T> {
    item: Option<T>,
    expires_at: Instant,
    expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
    shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
}

impl<T> Slot<T> {
    fn empty() -> Self {
        Self {
            item: None,
            expires_at: slot_sentinel(),
            expiry_channel: None,
            shutdown_channel: None,
        }
    }

    fn is_occupied(&self) -> bool {
        self.item.is_some()
    }

    fn populate(
        &mut self,
        item: T,
        expires_at: Instant,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) {
        self.item = Some(item);
        self.expires_at = expires_at;
        self.expiry_channel = expiry_channel;
        self.shutdown_channel = shutdown_channel;
    }

    fn take(&mut self) -> Option<PopResult<T>> {
        Some(PopResult {
            item: self.item.take()?,
            expires_at: self.expires_at,
            expiry_channel: self.expiry_channel.take(),
            shutdown_channel: self.shutdown_channel.take(),
        })
    }
}

/// Vec-backed circular buffer for bounded channel storage.
///
/// All operations are synchronous. The buffer knows nothing about wakeups,
/// notifications, or async. Concurrency protection is the caller's
/// responsibility.
pub(crate) struct Ring<T> {
    slots: Vec<Slot<T>>,
    head: usize,
    tail: usize,
    len: usize,
    target_capacity: usize,
    ttl: Duration,
    shutdown: bool,
    mode: ChannelMode,
    expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
    shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    /// True when the deadlines across occupied slots may be non-monotonic in
    /// FIFO order. Set by `set_ttl` only when the new TTL is strictly smaller
    /// than the current value AND the ring is non-empty. Cleared only by
    /// `drain_expired` after observing monotonic survivors. Never cleared by
    /// `set_ttl`: a TTL increase cannot repair an already-non-monotonic ring.
    ttl_reduced: bool,
}

impl<T> Ring<T> {
    pub(crate) const MIN_TTL: Duration = Duration::from_millis(1);
    pub(crate) const MAX_TTL: Duration = Duration::from_secs(365 * 24 * 60 * 60);

    /// Creates a new ring buffer with the given capacity, TTL, mode, and
    /// ring-level channels.
    ///
    /// Capacity is clamped to a minimum of 1. TTL must be within the inclusive
    /// range 1ms..=1 year.
    pub fn new(
        capacity: usize,
        ttl: Duration,
        mode: ChannelMode,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<Self, CaducusError> {
        Self::validate_ttl(ttl)?;
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(Slot::empty());
        }
        Ok(Self {
            slots,
            head: 0,
            tail: 0,
            len: 0,
            target_capacity: capacity,
            ttl,
            shutdown: false,
            mode,
            expiry_channel,
            shutdown_channel,
            ttl_reduced: false,
        })
    }

    /// SPSC push. Rejects with `InvalidPattern(item)` in MPSC mode.
    ///
    /// Populates the slot with `None` for both per-slot channel fields.
    /// Computes `expires_at` from the ring's own TTL.
    pub fn try_push_spsc(&mut self, item: T) -> Result<(), CaducusError<T>> {
        if self.mode != ChannelMode::Spsc {
            return Err(CaducusError {
                kind: CaducusErrorKind::InvalidPattern(item),
            });
        }
        self.push_common(item, None, None)
    }

    /// MPSC push. Rejects with `InvalidPattern(item)` in SPSC mode.
    ///
    /// Populates the slot with the provided per-slot channel handles.
    /// Computes `expires_at` from the ring's own TTL.
    pub fn try_push_mpsc(
        &mut self,
        item: T,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<(), CaducusError<T>> {
        if self.mode != ChannelMode::Mpsc {
            return Err(CaducusError {
                kind: CaducusErrorKind::InvalidPattern(item),
            });
        }
        self.push_common(item, expiry_channel, shutdown_channel)
    }

    /// Shared push logic: checks shutdown/full, computes expiry, populates slot.
    fn push_common(
        &mut self,
        item: T,
        expiry_channel: Option<Arc<dyn ReportChannel<T>>>,
        shutdown_channel: Option<Arc<dyn ReportChannel<T>>>,
    ) -> Result<(), CaducusError<T>> {
        if self.shutdown {
            return Err(CaducusError {
                kind: CaducusErrorKind::Shutdown(item),
            });
        }
        if self.len >= self.target_capacity {
            return Err(CaducusError {
                kind: CaducusErrorKind::Full(item),
            });
        }
        let now = Instant::now();
        let expires_at = now
            .checked_add(self.ttl())
            .or_else(|| now.checked_add(Self::MIN_TTL))
            .unwrap_or(now);
        self.slots[self.tail].populate(item, expires_at, expiry_channel, shutdown_channel);
        self.tail = (self.tail + 1) % self.slots.len();
        self.len += 1;
        Ok(())
    }

    /// Removes and returns the head item with its metadata.
    ///
    /// In SPSC mode, the returned `PopResult` is populated with clones of the
    /// ring-level channel handles. In MPSC mode, per-slot handles are used.
    /// Returns `None` if the buffer is empty. If a deferred shrink is pending
    /// and occupancy has dropped to target capacity, the buffer compacts.
    pub fn try_pop(&mut self) -> Option<PopResult<T>> {
        if self.len == 0 {
            return None;
        }
        let mut result = self.slots[self.head].take();
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        if self.should_compact() {
            self.compact();
        }
        if let Some(ref mut pop) = result {
            if self.mode == ChannelMode::Spsc {
                pop.expiry_channel = self.expiry_channel.clone();
                pop.shutdown_channel = self.shutdown_channel.clone();
            }
        }
        result
    }

    /// Returns the soonest expiry deadline among occupied slots, or `None`
    /// when empty.
    ///
    /// When `ttl_reduced` is clear, deadlines are FIFO-monotonic and the head
    /// is always the soonest. When `ttl_reduced` is set, deadlines may be
    /// non-monotonic; this method scans all occupied slots and returns the
    /// minimum. The flag is not mutated here — only `drain_expired` clears it.
    pub fn peek_expires_at(&self) -> Option<Instant> {
        if self.len == 0 {
            return None;
        }
        if !self.ttl_reduced {
            return Some(self.slots[self.head].expires_at);
        }
        let cap = self.slots.len();
        let mut min_deadline = self.slots[self.head].expires_at;
        for i in 1..self.len {
            let idx = (self.head + i) % cap;
            let d = self.slots[idx].expires_at;
            if d < min_deadline {
                min_deadline = d;
            }
        }
        Some(min_deadline)
    }

    /// Pops expired items and returns them in FIFO order.
    ///
    /// When `ttl_reduced` is clear, scans only the contiguous expired head
    /// prefix and stops at the first live head — no compaction needed. When
    /// the flag is set, scans every occupied slot, removes expired items in
    /// FIFO order, compacts via `linearize(target_capacity)` if a gap was
    /// opened among survivors, and clears the flag if the survivors are then
    /// FIFO-monotonic.
    pub fn drain_expired(&mut self, now: Instant) -> Vec<PopResult<T>> {
        if !self.ttl_reduced {
            // Fast path: contiguous head prefix only.
            let mut items = Vec::new();
            while let Some(expires_at) = self.slots.get(self.head).map(|s| s.expires_at) {
                if self.len == 0 || expires_at > now {
                    break;
                }
                if let Some(pop) = self.try_pop() {
                    items.push(pop);
                } else {
                    break;
                }
            }
            return items;
        }

        // Full scan: walk all occupied slots, removing any expired item in
        // FIFO order. Track whether a gap is opened among survivors.
        let cap = self.slots.len();
        let original_len = self.len;
        let mut items: Vec<PopResult<T>> = Vec::new();
        let mut gap_opened = false;
        let mut survivor_seen = false;
        let mut prev_survivor_deadline: Option<Instant> = None;
        let mut survivors_monotonic = true;

        for i in 0..original_len {
            let idx = (self.head + i) % cap;
            debug_assert!(self.slots[idx].is_occupied());
            let expires_at = self.slots[idx].expires_at;
            if expires_at <= now {
                if let Some(mut pop) = self.slots[idx].take() {
                    if self.mode == ChannelMode::Spsc {
                        pop.expiry_channel = self.expiry_channel.clone();
                        pop.shutdown_channel = self.shutdown_channel.clone();
                    }
                    items.push(pop);
                    self.len -= 1;
                    if survivor_seen {
                        gap_opened = true;
                    }
                }
            } else {
                survivor_seen = true;
                if let Some(prev) = prev_survivor_deadline {
                    if expires_at < prev {
                        survivors_monotonic = false;
                    }
                }
                prev_survivor_deadline = Some(expires_at);
            }
        }

        if gap_opened {
            // Compact survivors into contiguous slots. `linearize` assumes
            // all `len` slots from `head` are occupied, which is not true
            // here because the drain walk left empty slots interspersed
            // with survivors. Build a new Vec by walking the original
            // occupied range and taking the still-occupied slots in FIFO
            // order.
            let new_cap = self.target_capacity;
            let mut new_slots: Vec<Slot<T>> = Vec::with_capacity(new_cap);
            for i in 0..original_len {
                let idx = (self.head + i) % cap;
                if self.slots[idx].is_occupied() {
                    let mut empty = Slot::empty();
                    std::mem::swap(&mut self.slots[idx], &mut empty);
                    new_slots.push(empty);
                }
            }
            let placed = new_slots.len();
            while new_slots.len() < new_cap {
                new_slots.push(Slot::empty());
            }
            self.slots = new_slots;
            self.head = 0;
            self.tail = placed % self.slots.len();
            self.target_capacity = new_cap;
            debug_assert_eq!(self.len, placed);
        } else {
            // No gap: only a contiguous head prefix expired. Advance head by
            // the count removed.
            let removed = original_len - self.len;
            self.head = (self.head + removed) % cap;
        }

        // Honour any pending deferred shrink now that occupancy may have
        // dropped to target.
        if self.should_compact() {
            self.compact();
        }

        // Clear the flag if survivors are monotonic. `len <= 1` is trivially
        // monotonic. Survivors-monotonic was tracked during the walk.
        if self.len <= 1 || survivors_monotonic {
            self.ttl_reduced = false;
        }

        items
    }

    /// Sets the shutdown flag and extracts all remaining items in FIFO order.
    ///
    /// Each `PopResult` is populated with channel handles following the same
    /// mode rules as `try_pop`. Calling shutdown on an already shut-down
    /// buffer returns an empty Vec.
    pub fn shutdown(&mut self) -> Vec<PopResult<T>> {
        if self.shutdown {
            return Vec::new();
        }
        self.shutdown = true;
        let mut items = Vec::with_capacity(self.len);
        while self.len > 0 {
            if let Some(mut pop) = self.slots[self.head].take() {
                if self.mode == ChannelMode::Spsc {
                    pop.expiry_channel = self.expiry_channel.clone();
                    pop.shutdown_channel = self.shutdown_channel.clone();
                }
                items.push(pop);
            }
            self.head = (self.head + 1) % self.slots.len();
            self.len -= 1;
        }
        self.head = 0;
        self.tail = 0;
        items
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Returns the current TTL.
    pub fn ttl(&self) -> Duration {
        self.ttl.clamp(Self::MIN_TTL, Self::MAX_TTL)
    }

    /// Updates the TTL. Future pushes use the new value.
    ///
    /// When the new TTL is strictly smaller than the current value AND the
    /// ring is non-empty, sets the `ttl_reduced` flag because subsequent
    /// pushes will produce deadlines smaller than those of items already in
    /// the ring, breaking FIFO-monotonicity. If the ring is empty, no
    /// existing deadlines are at risk so the flag stays clear. A TTL increase
    /// never sets the flag and never clears it (an increase cannot repair an
    /// already-non-monotonic ring; it must wait for offending items to drain).
    pub fn set_ttl(&mut self, ttl: Duration) -> Result<(), CaducusError> {
        Self::validate_ttl(ttl)?;
        if ttl < self.ttl && self.len > 0 {
            self.ttl_reduced = true;
        }
        self.ttl = ttl;
        Ok(())
    }

    /// Requests a new capacity. Clamped to a minimum of 1.
    ///
    /// Growth reallocates immediately. Shrink reallocates immediately if
    /// occupancy allows, otherwise defers until pops bring occupancy down.
    pub fn request_capacity(&mut self, new: usize) {
        let new = new.max(1);
        let current = self.slots.len();
        if new == current && self.target_capacity == current {
            return;
        }
        if new > current {
            // Growth: reallocate and linearize.
            self.target_capacity = new;
            self.linearize(new);
        } else if new < current {
            self.target_capacity = new;
            if self.len <= new {
                // Immediate shrink.
                self.linearize(new);
            }
            // Otherwise deferred: compaction happens in try_pop.
        } else {
            // new == current but target_capacity differs (e.g. cancelling a pending shrink).
            self.target_capacity = new;
        }
    }

    /// Whether deferred compaction should run.
    fn should_compact(&self) -> bool {
        self.target_capacity < self.slots.len() && self.len <= self.target_capacity
    }

    /// Compacts the buffer to target_capacity by linearizing into a new Vec.
    fn compact(&mut self) {
        self.linearize(self.target_capacity);
    }

    /// Linearizes occupied slots into a new Vec of the given size,
    /// preserving FIFO order. Resets head to 0 and tail to len.
    ///
    /// Precondition: the first `self.len` slots from `self.head` must all be
    /// occupied. Callers that may have introduced gaps (e.g. the gap-aware
    /// drain in `drain_expired`) must perform their own gap-tolerant
    /// compaction instead of calling this helper.
    fn linearize(&mut self, new_capacity: usize) {
        let mut new_slots = Vec::with_capacity(new_capacity);
        let old_len = self.slots.len();
        for i in 0..self.len {
            let idx = (self.head + i) % old_len;
            debug_assert!(
                self.slots[idx].is_occupied(),
                "linearize precondition violated: slot at logical position {i} (index {idx}) is empty"
            );
            let mut empty = Slot::empty();
            std::mem::swap(&mut self.slots[idx], &mut empty);
            new_slots.push(empty);
        }
        for _ in self.len..new_capacity {
            new_slots.push(Slot::empty());
        }
        self.slots = new_slots;
        self.head = 0;
        self.tail = self.len % self.slots.len();
        self.target_capacity = new_capacity;
    }

    fn validate_ttl(ttl: Duration) -> Result<(), CaducusError> {
        if !(Self::MIN_TTL..=Self::MAX_TTL).contains(&ttl) {
            return Err(CaducusError {
                kind: CaducusErrorKind::InvalidArgument,
            });
        }
        Ok(())
    }
}
