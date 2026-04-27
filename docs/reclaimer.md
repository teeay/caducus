# Reclaimer Capability

Status: Developed

## Objectives

- Own the reclaimer task that removes expired items from the buffer.
- Own all item reporting logic (expiry and shutdown) so sender and receiver share one path.
- Serve the receiver through `try_receive`, which drains expired items and claims the next live
  item in a single call.
- Keep reporting outside the mutex, single-attempt, no backpressure.

## Technical Details

### Module

`src/reclaimer.rs`, crate-private module.

Uses `ConcurrentRing<T>`, `DrainMode`, `PopResult<T>`, and `ReportChannel<T>` from the
concurrency layer.

### Reporting

Two reporting functions. Both follow the same contract: single-attempt delivery through the
channel on the `PopResult`. On failure or panic, log a warning and drop the item. No retry.

All calls to user-provided `ReportChannel::send` are wrapped in `catch_unwind` with
`AssertUnwindSafe`. A panicking channel implementation cannot kill the reclaimer task or unwind
through a destructor (which would abort the process).

**`report_expired(items: Vec<PopResult<T>>)`**

For each `PopResult`, calls `expiry_channel.send(item)`. Used by the reclaimer task after draining
expired items, and by `try_receive` on the receiver path.

- In SPSC mode, `expiry_channel` is a clone of the ring-level handle (populated by `try_pop`),
  which may be `None` if the SPSC builder was not configured with an expiry channel.
- In MPSC mode, `expiry_channel` is the per-slot handle stored at send time.
- If `expiry_channel` is `None` (in either mode), the item is dropped silently.

**`report_shutdown(items: Vec<PopResult<T>>)`**

For each `PopResult`, calls `shutdown_channel.send(item)`. Used by sender `shutdown()`, sender
`Drop`, and receiver `Drop` after `ConcurrentRing::shutdown()` drains the buffer.

- In SPSC mode, `shutdown_channel` is a clone of the ring-level handle, which may be `None` if
  the SPSC builder was not configured with a shutdown channel.
- In MPSC mode, `shutdown_channel` is the per-slot handle stored at send time.
- If `shutdown_channel` is `None` (in either mode), the item is dropped silently.

**`shutdown_and_report(ring: &ConcurrentRing<T>)`**

Convenience wrapper that calls `ring.shutdown()` to drain the buffer and then `report_shutdown`
on the returned items. The concurrency layer releases the ring mutex inside `shutdown()` before
returning the items, so reporting always happens outside the lock. Used by `SpscSender::shutdown`,
`SpscSender::drop`, `MpscSender::shutdown`, the last-sender branch of `MpscSender::drop`, and
`Receiver::drop`. `report_shutdown` remains available for call sites that already hold drained
items (e.g. the reclaimer task on shutdown exit).

#### Drop-Path Reporting

`report_shutdown` runs synchronously on the dropping thread when reached through
`SpscSender::drop`, the last `MpscSender::drop`, or `Receiver::drop`. Every still-buffered item
is delivered to its `shutdown_channel` inline before `Drop` returns. `catch_unwind` protects
the destructor from a panicking `send`, but it does not protect against blocking I/O, slow
computation, or a channel that waits on an external condition. A blocking `ReportChannel::send`
will stall the dropping thread for the full drain.

`ReportChannel::send` implementations passed to Caducus must therefore be synchronous,
non-blocking, and bounded in cost. Callers who need to control when reporting work happens
should call `shutdown()` on the sender (or drop senders before the receiver) so the destructor
finds an empty buffer.

See `docs/sender.md` `### Sender Drop` and `docs/receiver.md` `### Receiver Drop` for the
sender and receiver sides of this contract.

### Receiver Support

**`try_receive(ring: &ConcurrentRing<T>) -> Result<Option<T>, CaducusError>`**

Called by `Receiver::next` on each iteration of its wait loop. Bridges the receiver to the
concurrency layer without exposing internal types.

1. Calls `ring.drain(Instant::now(), DrainMode::DrainAndClaim)`.
2. Reports any expired items through `report_expired`.
3. Returns:
   - `Ok(Some(item))` -- a live item was claimed.
   - `Ok(None)` -- buffer is empty, caller should wait.
   - `Err(Shutdown(()))` -- buffer is shut down and empty.

### Reclaimer Task

A detached Tokio task spawned by the builder's `build()` method. It is the sole mechanism for
removing expired items from the buffer under normal operation.

**`spawn_reclaimer(ring: Weak<ConcurrentRing<T>>, notify_reclaimer: Arc<Notify>, notify_receiver: Arc<Notify>, handle: &Handle)`**

Spawns the reclaimer task on the provided runtime handle. Called once during `build()`.

**State:**

```
Weak<ConcurrentRing<T>>        // loses upgrade when all strong references drop
Arc<tokio::sync::Notify>       // reclaimer wakeup handle
Arc<tokio::sync::Notify>       // receiver wakeup handle (to signal exposed live items)
```

**Loop:**

1. Attempt `Weak::upgrade()`. If it fails, all strong references dropped -- exit.
2. Create `Notify` waiter via `notify_reclaimer.notified()`. The waiter captures the Notify's
   current version counter.
3. Call `ConcurrentRing::drain(Instant::now(), DrainMode::DrainOnly)`:
   - Returns `DrainResult` with `expired`, `live` (always `None`), `next_deadline`, `is_shutdown`.
   - Under TTL-shrink transients the underlying drain scans all occupied slots and the
     `next_deadline` reflects the minimum remaining deadline; otherwise the drain pops the
     contiguous expired head prefix and `next_deadline` is the head's deadline. Wakeup
     correctness is preserved because `next_deadline` always points at the soonest remaining
     deadline.
4. Drop the strong reference before reporting.
5. If `is_shutdown` is `true`: call `report_expired(expired)`, then exit.
6. Call `report_expired(expired)`.
7. If expired items were drained and a live head is now exposed (`next_deadline.is_some()`):
   call `notify_receiver.notify_waiters()` to wake a blocked receiver.
8. Check the returned deadline:
   - Deadline already reached: loop immediately to step 1.
   - Deadline in the future: `tokio::select!` between the waiter and
     `tokio::time::sleep_until(deadline)`.
   - No deadline (buffer empty): `waiter.await`.
9. On wake: go to step 1.

**Wakeup correctness:** The waiter is created at step 2, before `drain` at step 3. This matters
only when the buffer is empty after drain (no deadline). If a sender pushes an item while the
reclaimer is reporting (step 6) or sleeping (step 8), `notify_waiters()` fires after the waiter
was created, so the waiter wakes.

**Exit conditions:**

- `Weak::upgrade()` fails (all strong references dropped).
- `drain` returns `is_shutdown == true`.

### Validation

- `tests/caducus.rs` covers: reclaimer removes expired items before receiver sees them, reclaimer
  exits on shutdown, reclaimer exits on all strong references dropped, expiry reporting
  single-attempt with warning on failure, shutdown reporting through shutdown channels on both
  sender-initiated and receiver-initiated shutdown, `try_receive` used by receiver's `next` loop,
  panicking expiry channel does not kill the reclaimer task, panicking shutdown channel in sender
  drop does not abort, panicking shutdown channel in receiver drop does not abort.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
