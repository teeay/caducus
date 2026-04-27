# Concurrency Layer

Status: Developed

## Objectives

- Serialise access to the ring buffer through one mutex.
- Coordinate wakeups between sender, receiver, and reclaimer via two `Notify` handles.
- Expose two send methods (SPSC and MPSC) that pass through to the corresponding ring buffer push
  variant. Mode enforcement is the ring buffer's responsibility.
- Provide a single unified `drain` method controlled by `DrainMode` for both the reclaimer task
  and the receiver path.

## Technical Details

### Shared State

`ConcurrentRing<T>` holds three fields:

- `Mutex<Ring<T>>` -- the single lock protecting all shared state.
- `Arc<Notify>` (reclaimer) -- wakes the reclaimer task when items are pushed, capacity changes,
  or shutdown occurs.
- `Arc<Notify>` (receiver) -- wakes a blocked receiver when a live item becomes available.

### Error Model

See `docs/caducus.md` for the error type definitions and variant catalogue. The concurrency layer
uses these types on all public-facing methods.

### Critical-Section Rules

- All operations follow the pattern: lock, delegate to ring buffer, unlock, then side effects
  (notify, report, drop payloads).
- `.await`, reporting, and payload drop never happen while the mutex is held.
- Mutex poisoning is recovered through `PoisonError::into_inner()`. The ring's structural
  invariants are maintained by its own operations regardless of whether a prior lock holder
  panicked.

### Send Path

Two send methods pass through to the corresponding ring buffer push variant. The concurrency layer
does not check the mode. Both methods fire both Notify handles after a successful push.

**`send_spsc(item: T) -> Result<(), CaducusError<T>>`**

1. Lock, call `ring.try_push_spsc(item)`.
2. On error, return (carries the item).
3. Unlock, notify reclaimer and receiver.

**`send_mpsc(item: T, expiry_channel, shutdown_channel) -> Result<(), CaducusError<T>>`**

1. Lock, call `ring.try_push_mpsc(item, expiry_channel, shutdown_channel)`.
2. On error, return (carries the item).
3. Unlock, notify reclaimer and receiver.

### Drain Path

**`drain(now: Instant, mode: DrainMode) -> DrainResult<T>`**

Single unified method for both the reclaimer task and the receiver path. Single lock acquisition.

`DrainMode` controls whether a live item is also claimed:

- `DrainOnly` -- drain expired heads only. Used by the reclaimer task.
- `DrainAndClaim` -- drain expired heads and pop the next live item. Used by the receiver path
  through `reclaimer::try_receive`.

Steps:

1. Lock.
2. Call `ring.drain_expired(now)` -- when the ring's `ttl_reduced` flag is clear, pops a
   contiguous prefix of expired heads (fast path); when set, scans every occupied slot, removes
   expired items in FIFO order, compacts via `linearize` if a gap was opened among survivors,
   and clears the flag if the surviving deadlines are then monotonic. See
   `docs/ring-buffer.md` `### Deadline Monotonicity` for the flag's lifecycle.
3. If `DrainAndClaim`, call `ring.try_pop()` to claim the next live item.
4. Read `ring.peek_expires_at()` for the next deadline. While the flag is set this returns the
   minimum deadline across all occupied slots; otherwise it returns the head deadline.
5. Read `ring.is_shutdown()`.
6. Unlock.
7. Return `DrainResult { expired, live, next_deadline, is_shutdown }`.

No side effects (no notify, no reporting) -- callers decide what to do with the result.

### Configuration

**`update_ttl(duration) -> Result<(), CaducusError>`** -- delegates to `ring.set_ttl(duration)`.
Future sends use the updated TTL. Already-enqueued items keep their original `expires_at`.

**`update_capacity(new: usize)`** -- delegates to `ring.request_capacity(new)`, then notifies
both handles.

### Shutdown

**`shutdown() -> Vec<PopResult<T>>`**

Lock, call `ring.shutdown()` (sets flag, drains items), unlock, notify both handles, return
drained items.

After shutdown: sends reject with `Shutdown(item)`, receives return `Shutdown(())`, reclaimer
exits.

### Notification Flow

| Event              | Who notifies     | Which handle        | Who may wake        |
|--------------------|------------------|---------------------|---------------------|
| Successful send    | Sender           | Both                | Receiver, Reclaimer |
| Expired drained    | Reclaimer        | Receiver            | Receiver            |
| Capacity update    | ConcurrentRing   | Both                | Receiver, Reclaimer |
| Shutdown           | ConcurrentRing   | Both                | Receiver, Reclaimer |

### Validation

- Send-then-drain returns the item (DrainAndClaim).
- FIFO ordering preserved through the concurrency layer.
- DrainOnly returns expired items but never claims a live item.
- DrainAndClaim skips expired heads and returns the first live item.
- DrainAndClaim returns `None` when all items are expired.
- DrainOnly drains multiple expired heads, stops at live head.
- Drain returns next deadline, `None` when empty.
- Drain returns shutdown flag.
- DrainAndClaim after DrainOnly: removing expired heads exposes live items.
- TTL update changes expiry for future sends, not existing items.
- TTL validation: construction and `update_ttl` reject values outside `1ms..=1 year`.
- SPSC send: `send_spsc` delegates to `ring.try_push_spsc`, returns `InvalidPattern` in MPSC mode.
- MPSC send: `send_mpsc` delegates to `ring.try_push_mpsc`, returns `InvalidPattern` in SPSC mode.
- Capacity update: `update_capacity` changes ring capacity under the lock.
- Notify handles: reclaimer and receiver handles are distinct `Arc<Notify>` instances.
- Poisoned mutex: recovered with `into_inner()`, operations proceed normally.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
