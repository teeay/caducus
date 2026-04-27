# Ring Buffer Storage Layer

Status: Developed

## Objectives

- Provide a Vec-backed bounded ring buffer as the storage layer for the channel.
- Support SPSC and MPSC operating modes, set at construction and immutable.
- Use pure move semantics for `T`: moved in on push, moved out on pop or shutdown.
- Slots start empty and are populated only by push.
- The slot structure is identical in both modes for predictable memory use.
- The layer is synchronous; concurrency protection is the caller's responsibility.

## Technical Details

### Structure

`Ring<T>` is a circular buffer backed by `Vec<Slot<T>>`. The Vec length is the capacity.

Ring-level metadata:

- `mode: ChannelMode` -- `Spsc` or `Mpsc`, set at construction, immutable.
- `head: usize` -- index of the oldest occupied slot (next pop position).
- `tail: usize` -- index of the next free slot (next push position).
- `len: usize` -- number of currently occupied slots.
- `target_capacity: usize` -- desired capacity; differs from `slots.len()` only during a pending
  shrink.
- `ttl: Duration` -- current time-to-live for newly enqueued items. `ttl()` is the single accessor
  for expiry calculation and returns the stored value clamped to the inclusive range `1ms..=1 year`.
- `shutdown: bool` -- whether shutdown has been initiated. Once set, pushes are permanently
  rejected. The flag is irreversible.
- `expiry_channel: Option<Arc<dyn ReportChannel<T>>>` -- ring-level expiry channel. Optionally
  set at construction in SPSC mode (may be `None`); always `None` in MPSC mode.
- `shutdown_channel: Option<Arc<dyn ReportChannel<T>>>` -- ring-level shutdown channel.
  Optionally set at construction in SPSC mode (may be `None`); always `None` in MPSC mode.

**`Ring::new(capacity, ttl, mode, expiry_channel, shutdown_channel) -> Result<Ring<T>, CaducusError>`**

Allocates a Vec of `capacity` empty slots and stores the provided TTL, mode, and ring-level
channels. Capacity is clamped to a minimum of 1. TTL must be within `1ms..=1 year`; otherwise
returns `CaducusErrorKind::InvalidArgument`.

### Slot

The slot structure is identical in both modes for predictable memory use.

Each `Slot<T>` holds:

- `item: Option<T>` -- the payload. `None` when empty, `Some(T)` when occupied.
- `expires_at: Instant` -- absolute expiry deadline, computed by the ring at push time. Empty
  slots store a process-wide sentinel `Instant` (lazily initialised on the first ring
  construction), not a fresh `Instant::now()` per slot. Construction and resize therefore avoid
  one clock read per slot. The sentinel is never compared against a real deadline because expiry
  checks only consider occupied slots.
- `expiry_channel: Option<Arc<dyn ReportChannel<T>>>` -- in MPSC mode, captured from the caller at
  push time. In SPSC mode, always `None`.
- `shutdown_channel: Option<Arc<dyn ReportChannel<T>>>` -- in MPSC mode, captured from the caller
  at push time. In SPSC mode, always `None`.

### Operations

#### Push

Two push variants enforce the correct send pattern for the configured mode. Using the wrong variant
returns `CaducusErrorKind::InvalidPattern(item)`, carrying the item back to the caller.

Both variants reject with `Shutdown(item)` if shut down, `Full(item)` if at capacity. Both compute
`expires_at` from the ring's own `ttl()` using `checked_add` with a `MIN_TTL` fallback to guarantee
a finite deadline.

**`try_push_spsc(item: T) -> Result<(), CaducusError<T>>`**

SPSC push. Rejects with `InvalidPattern(item)` in MPSC mode. Populates the slot at `tail` with
`None` for both per-slot channel fields and advances `tail` with wrapping.

**`try_push_mpsc(item: T, expiry_channel, shutdown_channel) -> Result<(), CaducusError<T>>`**

MPSC push. Rejects with `InvalidPattern(item)` in SPSC mode. Populates the slot at `tail` with
the provided per-slot channel handles and advances `tail` with wrapping.

#### Pop

**`try_pop() -> Option<PopResult<T>>`**

Takes the item and metadata from the slot at `head`, clears the slot, advances `head` with
wrapping. In SPSC mode, `PopResult` is populated with clones of the ring-level channel handles. In
MPSC mode, `PopResult` carries the per-slot handles. Returns `None` if empty. If a deferred shrink
is pending and `len` has dropped to `target_capacity`, the buffer compacts.

**`peek_expires_at() -> Option<Instant>`**

Returns the soonest expiry deadline among occupied slots, or `None` when empty. When the
`ttl_reduced` flag is clear, deadlines are FIFO-monotonic and this returns the head slot's
`expires_at`. When the flag is set, deadlines may be non-monotonic so this scans all occupied
slots and returns the minimum. Does not mutate the flag.

#### TTL

**`ttl() -> Duration`** -- returns the current TTL clamped to `1ms..=1 year`.

**`set_ttl(duration) -> Result<(), CaducusError>`** -- updates the TTL. Future pushes use the new
value. Values outside `1ms..=1 year` are rejected with `CaducusErrorKind::InvalidArgument`.

When the new TTL is strictly smaller than the current value AND the ring is non-empty, also sets
the `ttl_reduced` flag (see `### Deadline Monotonicity`). A reduction on an empty ring or any TTL
increase leaves the flag unchanged.

### Deadline Monotonicity

The ring tracks a `ttl_reduced` boolean alongside the other metadata. When clear, deadlines
across occupied slots are guaranteed FIFO-monotonic: pushes computed under a non-decreasing TTL
sequence always produce non-decreasing deadlines, and `peek_expires_at` and `drain_expired` use
fast head-only paths.

When set, deadlines may not be FIFO-monotonic — a TTL reduction with items already in the ring
followed by further pushes can produce later items with earlier deadlines. The flag is set by
`set_ttl` only when the new TTL is strictly smaller than the current value AND the ring is
non-empty. A reduction on an empty ring is harmless because future pushes will all use the new
TTL. A TTL increase never sets the flag and never clears it: an increase cannot repair an
already-non-monotonic ring (older items still hold their shorter deadlines).

The flag is cleared only by `drain_expired` after a full-scan walk observes that the survivors
are FIFO-monotonic, or trivially when `len <= 1` after the drain. Once cleared, future sends
preserve monotonicity until the next TTL reduction.

While the flag is set, `peek_expires_at` returns the minimum deadline across all occupied slots,
and `drain_expired` performs a full occupied-slot scan with conditional compaction. Both behave
as before once the flag clears.

#### Shutdown

**`shutdown() -> Vec<PopResult<T>>`**

Sets the shutdown flag and extracts all remaining items in FIFO order. Each `PopResult` is
populated with channel handles following the same mode rules as `try_pop`. After shutdown, the
buffer is empty with `head` and `tail` reset to 0. The storage Vec is left at its current
allocated size — shutdown is a terminal operation, so reallocating just to release the old Vec
slightly earlier than the surrounding `Arc` would be net-negative work. Any pending deferred
shrink is therefore not honoured by `shutdown`. Calling shutdown on an already shut-down buffer
returns an empty Vec.

#### Accessors

- `len()` -- number of occupied slots.
- `capacity()` -- `slots.len()`.
- `target_capacity()` -- effective capacity limit; equals `capacity()` when no shrink is pending.
- `is_empty()` -- `len == 0`.
- `is_full()` -- `len >= target_capacity`.
- `is_shutdown()` -- returns the shutdown state.

### PopResult

`PopResult<T>` carries the extracted item and its associated metadata:

- `item: T`
- `expires_at: Instant`
- `expiry_channel: Option<Arc<dyn ReportChannel<T>>>`
- `shutdown_channel: Option<Arc<dyn ReportChannel<T>>>`

Callers see a uniform interface regardless of mode.

### Resize

**`request_capacity(new: usize)`**

`new` is clamped to a minimum of 1.

- **Growth** (`new > slots.len()`): allocate a new Vec of size `new`, linearize existing items into
  contiguous slots starting at index 0, set `head = 0`, `tail = len % new`, update
  `target_capacity` to `new`.
- **Immediate shrink** (`new < slots.len()` and `len <= new`): allocate a new Vec of size `new`,
  linearize, update metadata identically to growth.
- **Deferred shrink** (`new < slots.len()` and `len > new`): set `target_capacity = new`. The
  buffer immediately stops accepting pushes beyond `target_capacity`. Compaction happens inside
  `try_pop` or `shutdown` once `len` drops to `target_capacity`.
- **No-op** (`new == slots.len()` and no pending shrink): nothing changes.

Linearization moves all occupied slots into contiguous positions `0..len` in the new Vec,
preserving FIFO order. Each slot's complete metadata moves with the item.

### Safety

- All operations are deterministic structural work on the Vec and index metadata.
- Report channel calls, payload drops, and logging are the caller's responsibilities after items
  leave storage.

### Validation

- FIFO ordering across push and pop sequences, including wrap-around.
- Bounded-full: push returns the item when `len >= target_capacity`.
- Growth: items survive reallocation and remain in FIFO order.
- Immediate shrink: items survive reallocation when `len <= new`.
- Deferred shrink: `is_full` respects `target_capacity` immediately; compaction completes on pop
  when `len` reaches `target_capacity`.
- Shutdown: sets flag, returns all remaining items in FIFO order, buffer is empty afterward.
- Shutdown rejects subsequent pushes.
- Shutdown is irreversible; repeated calls return an empty Vec.
- TTL validation: `Ring::new` and `set_ttl` reject values outside `1ms..=1 year`.
- TTL accessor: `ttl()` clamps the stored value before returning.
- Peek: correct head `expires_at` visibility, `None` when empty.
- No-op resize: `request_capacity` with current capacity and no pending shrink does nothing.
- Cancel deferred shrink by growing: a growth request during a pending shrink overrides the target
  and reallocates immediately.
- Metadata preservation: `expires_at` and report channel handles survive the drain in shutdown
  and survive linearization across growth and shrink.
- Shutdown does not compact storage: any pending deferred shrink is left unapplied.
- Capacity clamping: `request_capacity(0)` becomes 1.
- Construction: Vec allocated with correct number of empty slots, TTL stored, metadata initialized.
- Mode immutability: mode is set at construction and cannot change.
- SPSC push pattern: `try_push_spsc` succeeds in SPSC mode, returns `InvalidPattern` in MPSC mode.
- MPSC push pattern: `try_push_mpsc` succeeds in MPSC mode, returns `InvalidPattern` in SPSC mode.
- SPSC `PopResult`: channel handles populated from ring-level fields on pop and shutdown.
- MPSC `PopResult`: channel handles populated from per-slot fields on pop and shutdown.
- SPSC per-slot channels: always `None` in storage regardless of ring-level channel state.
- `ttl_reduced` flag set by `set_ttl` on strict reduction with non-empty ring.
- `ttl_reduced` flag not set by `set_ttl` on reduction with empty ring.
- `ttl_reduced` flag not set by `set_ttl` on increase, and not cleared by `set_ttl` on increase.
- `drain_expired` head-only fast path when flag is clear.
- `drain_expired` full-scan path when flag is set: removes expired items in FIFO order, compacts
  if a gap was opened among survivors, and clears the flag if survivors are monotonic afterwards.
- TTL-shrink with non-head expired items: drain returns the expired items in FIFO order while
  the surviving head remains receivable.
- `peek_expires_at` returns head deadline when flag is clear, minimum when set, never mutates the
  flag.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
