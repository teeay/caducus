# Receiver Capability

Status: Developed

## Objectives

- Define receiver behavior for the single-consumer side of the bounded channel.
- Receiver behavior is identical in SPSC and MPSC modes.
- Keep receiver logic thin over the reclaimer layer.

## Technical Details

### Receiver Struct

```
Receiver<T> {
    ring: Arc<ConcurrentRing<T>>,
    notify: Arc<tokio::sync::Notify>,
}
```

Mode-agnostic. One receiver per channel, not cloneable. Stores the shared `Arc` to the
concurrency layer and the receiver's `Notify` handle.

### next

**`next(deadline: Option<Instant>) -> Result<T, CaducusError>`**

This is the only way data leaves the channel for the caller. It returns the owned `T`, never
internal types. The receiver owns the wait loop and timeout logic.

Passing `None` uses `DEFAULT_RECEIVE_TIMEOUT` (1 second). `None` does not mean wait indefinitely.

The full call path:

1. Compute effective deadline: `deadline.unwrap_or_else(|| Instant::now() + DEFAULT_RECEIVE_TIMEOUT)`.

2. **First check** -- calls `reclaimer::try_receive(&self.ring)`, which:
   - Calls `ConcurrentRing::drain(now, DrainMode::DrainAndClaim)`.
   - Reports any expired items through `reclaimer::report_expired`.
   - Returns `Ok(Some(item))` if a live item was claimed, `Ok(None)` if the buffer is empty,
     or `Err(Shutdown(()))` if the buffer is shut down and empty.

3. If a live item is available, returns `Ok(item)`. If shutdown, returns `Err(Shutdown(()))`.

4. **Double-check Notify pattern** -- creates a `Notify` waiter *before* rechecking, preventing
   lost wakeups between the recheck and the wait.

5. **Recheck** -- calls `reclaimer::try_receive` again. If a live item is available or shutdown,
   returns immediately.

6. **Wait** -- computes `remaining = deadline - now`. If zero, returns `Err(Timeout)`.
   `tokio::select!` between the waiter and `tokio::time::sleep(remaining)`:
   - Waiter fires (sender pushed an item, reclaimer exposed a live head, or shutdown signalled):
     loops back to step 2.
   - Sleep fires (deadline reached): returns `Err(Timeout)`.

The timeout is a **total deadline**, computed once at entry. It is not reset on spurious wakes or
expired-head drains. Each iteration uses the remaining time against the original deadline.

### Expired-Head Liveness Fallback

When the receiver encounters expired heads during its check, they are drained by
`reclaimer::try_receive` using `ConcurrentRing::drain` and reported through
`reclaimer::report_expired`, maintaining the conservation invariant. This guarantees forward
progress even when the reclaimer is no longer running (e.g. all senders dropped).

Under normal operation, the reclaimer removes expired items before the receiver sees them. The
receiver only hits this path when the reclaimer has exited or has not yet run.

### What The Caller Never Sees

- Expired items: removed by the reclaimer or by the receiver's expired-head fallback, and reported
  through expiry channels in both cases.
- Shutdown-drained items: drained by `shutdown()` and reported through shutdown channels.
- Internal types: `PopResult` is consumed by the reclaimer layer. The caller gets `T`.

### Shutdown Behavior

When shutdown has been called (by sender, sender drop, or by receiver drop) and the buffer is
empty, `reclaimer::try_receive` returns `Err(Shutdown(()))`. If items remain at the moment of
shutdown, they are drained by the shutdown call itself -- the receiver does not get them.

### Receiver Drop

Receiver drop triggers shutdown through the concurrency layer by calling
`reclaimer::shutdown_and_report(&self.ring)`, which:

1. Calls `ConcurrentRing::shutdown()` -- sets shutdown flag, drains all remaining items, notifies
   all waiters.
2. Reports the drained items through each item's `shutdown_channel` (see `docs/reclaimer.md`).

After drop, sender enqueue attempts fail with `Shutdown(item)`.

#### Synchronous Drop Reporting

Both steps run synchronously on the dropping thread. Every still-buffered item is delivered to
its `shutdown_channel` inline before `Receiver::drop` returns. The same contract applies as on
the sender side: `ReportChannel::send` implementations must be synchronous, non-blocking, and
bounded in cost because they may execute inside `Drop`. `catch_unwind` protects against panic
but not against blocking I/O or external waits.

To control when reporting work happens, drop the senders or call `shutdown()` on them first so
that a `Receiver::drop` running afterwards finds an empty buffer.

See `docs/sender.md` and `docs/reclaimer.md` for the sender and reporting sides of this
contract.

### Validation

- `tests/caducus.rs` covers: FIFO receive ordering, timeout behavior (total deadline semantics),
  shutdown behavior (receiver gets `Shutdown(())` after shutdown + empty), receiver closure causes
  sender `Shutdown(item)`, expired items excluded from returned values, default 1-second timeout
  when `None` is passed.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
