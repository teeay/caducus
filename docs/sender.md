# Sender Capability

Status: Developed

## Objectives

- Define sender behavior for the bounded asynchronous channel.
- Support two distinct sender types: `SpscSender<T>` (not cloneable) and `MpscSender<T>`
  (cloneable with channel snapshot semantics).
- Add per-item expiry send variants that allow callers to override the configured default TTL for
  individual sends.

## Technical Details

### Builders

Both builders share the same shape: `new(capacity, ttl)` plus optional `expiry_channel(...)`
and `shutdown_channel(...)` methods. `capacity` is clamped to a minimum of 1. `ttl` must be
within `1ms..=1 year`; out-of-range values return `CaducusErrorKind::InvalidArgument`.

**`SpscBuilder<T>::new(capacity, ttl)`**

Configures an SPSC queue. Optional methods `expiry_channel(...)` and `shutdown_channel(...)`
set the ring-level report channels. When a channel is not configured, the corresponding outcome
(expiry or shutdown) is silently dropped, matching MPSC's `None`-channel semantics.

**`MpscBuilder<T>::new(capacity, ttl)`**

Configures an MPSC queue. Optional methods `expiry_channel(...)` and `shutdown_channel(...)` set the
initial sender-local report channels. The first `MpscSender<T>` uses these channels, and each
subsequent clone snapshots the channels configured on the source sender at clone time.

SPSC stores report channels at ring level. MPSC stores report channels on each sender and copies
them into each item at send time. In both modes, absent channels silently drop the corresponding
expiry or shutdown outcome.

Both builders accept an optional Tokio runtime handle via `runtime(handle)`. If not provided,
`build()` calls `Handle::try_current()` to obtain the ambient runtime.

**`build()` (both builders)**

1. Uses the provided `Handle`, or calls `Handle::try_current()`. Returns
   `CaducusErrorKind::NoRuntime` if neither yields a runtime.
2. Constructs `ConcurrentRing<T>` with the configured capacity, TTL, mode, and channels.
3. Wraps the `ConcurrentRing` in `Arc`.
4. Spawns the reclaimer task via `reclaimer::spawn_reclaimer()`, passing both Notify handles
   (see `docs/reclaimer.md`).
5. Returns `(SpscSender<T>, Receiver<T>)` or `(MpscSender<T>, Receiver<T>)`.

### SpscSender

```
SpscSender<T> {
    ring: Arc<ConcurrentRing<T>>,
}
```

Not cloneable. Report channels, if configured at the builder, are fixed at construction inside
the ring buffer; an unconfigured channel results in silent drop of the corresponding outcome.

### MpscSender

```
MpscSender<T> {
    ring: Arc<ConcurrentRing<T>>,
    expiry_channel: Mutex<Option<Arc<dyn ReportChannel<T>>>>,
    shutdown_channel: Mutex<Option<Arc<dyn ReportChannel<T>>>>,
    sender_count: Arc<AtomicUsize>,
}
```

Cloneable. Cloning increments the shared `sender_count` and snapshots both channel handles
atomically (both locks held). Later `set_expiry_channel`, `set_shutdown_channel`, or
`set_channels` calls affect only that clone's future sends.

### Send Path

Both sender types expose a plain `send(item)` method. The internal delegation differs by mode.

**`SpscSender::send(item) -> Result<(), CaducusError<T>>`**

1. Calls `ConcurrentRing::send_spsc(item)`.
2. Concurrency layer: acquires mutex, calls `Ring::try_push_spsc(item)`.
3. Ring buffer: validates mode is SPSC (otherwise `InvalidPattern(item)`).
4. Ring buffer: calls `push_common` with no supplied deadline.
5. `push_common`: rejects if shutdown (`Shutdown(item)`) or full (`Full(item)`), resolves
   `expires_at = now + configured ttl`, tracks deadline
   ordering, writes item, expiry, and `None` for both per-slot channel fields into the slot at
   `tail`, advances `tail`, and increments `len`.
6. Concurrency layer: releases mutex, notifies both reclaimer and receiver.
7. Returns `Ok(())` to the caller. On any error, the item is returned inside the error variant.

**`MpscSender::send(item) -> Result<(), CaducusError<T>>`**

1. Snapshots both sender-local channels atomically (both locks held, then released).
2. Calls `ConcurrentRing::send_mpsc(item, expiry_channel, shutdown_channel)`.
3. Concurrency layer: acquires mutex, calls `Ring::try_push_mpsc(item, expiry_channel,
   shutdown_channel)`.
4. Ring buffer: validates mode is MPSC (otherwise `InvalidPattern(item)`), then calls
   `push_common` with no supplied deadline.
5. `push_common`: same checks as SPSC. Resolves `expires_at = now + configured ttl`, tracks
   deadline ordering, writes item, expiry, and the
   provided per-slot channel handles into the slot at `tail`, advances `tail`, and increments
   `len`.
6. Concurrency layer: releases mutex, notifies both reclaimer and receiver.
7. Returns `Ok(())` to the caller. On any error, the item is returned inside the error variant.

### Per-Item Expiry Send

Both sender types expose per-item expiry variants alongside the default `send(item)`:

```
send_with_ttl(item, ttl: Duration) -> Result<(), CaducusError<T>>
send_with_deadline(item, deadline: Instant) -> Result<(), CaducusError<T>>
```

`send(item)` remains the default path and uses the channel's configured TTL. `send_with_ttl`
validates `ttl` against the same library limits as builder construction and `update_ttl`
(`1ms..=1 year`). Values outside that range reject the send with `InvalidTTL(item)` and must
return the rejected item to the caller.

`send_with_deadline` accepts any future `Instant`, without applying the configured TTL limits. A
deadline that is at or before the validation-time `Instant::now()` rejects the send with an
`InvalidTTL(item)` and must return the rejected item to the caller. The system does not otherwise
judge whether an absolute deadline is sensible for the application; it only rejects deadlines that
would already be expired if enqueued. A valid future deadline may be arbitrarily far in the future;
that is caller policy.

The sender layer validates per-item TTLs and deadlines before taking the ring mutex. In MPSC mode,
validation also happens before report-channel snapshot locks are taken, so invalid per-item sends
do not serialise behind either the channel configuration locks or the ring mutex. A valid per-item
TTL is converted to an absolute `expires_at: Instant`; a valid deadline is passed through as that
same absolute deadline.

The ring exposes one shared insertion primitive with an optional deadline. `None` means use the
configured default TTL; `Some(expires_at)` means use the caller-supplied expiry that the sender
already validated. The ring remains the single shutdown/full gate and performs those checks exactly
once after the mode-specific push variant has selected the insertion path.

SPSC and MPSC must expose equivalent per-item expiry APIs. MPSC variants keep the existing
sender-local report-channel snapshot semantics: after resolving and validating the deadline, the
sender snapshots its current expiry and shutdown channels and stores those handles with the item at
send time.

Per-item expiry does not make Caducus a fairness scheduler. Receive delivery remains strictly FIFO:
the receiver claims the oldest live item, not the item with the earliest expiry deadline. The
reclaimer may remove expired non-head items when deadlines are non-monotonic, but live items are
not reordered by expiry. Callers that need earliest-deadline-first, priority, or fair scheduling
must implement that policy before sending into Caducus.

### Report Channel Configuration

`set_expiry_channel(ch)` and `set_shutdown_channel(ch)` update a single channel each.
`set_channels(expiry, shutdown)` updates both atomically (both locks held), preventing a
concurrent `send` from observing a mixed channel pair.

### Configuration

`update_capacity(new)` and `update_ttl(duration)` forward to the concurrency layer. `update_ttl`
rejects values outside `1ms..=1 year` with `InvalidArgument`. Capacity is clamped to a minimum
of 1. TTL changes apply to future sends only; already-enqueued items keep their original
`expires_at`. Capacity changes notify waiters.

### Shutdown

`shutdown()` delegates to `reclaimer::shutdown_and_report(&self.ring)`, which calls
`ConcurrentRing::shutdown()` to set the shutdown flag, drain all items, and notify waiters,
then reports the drained items through `reclaimer::report_shutdown`.

After shutdown: subsequent sends return `Shutdown(item)`. The receiver's `next` returns
`Shutdown(())`. The reclaimer wakes and exits.

See `docs/reclaimer.md` for reporting semantics.

### is_closed

Returns `true` when the receiver has dropped (triggering shutdown) or `shutdown()` has been called.
Delegates to `ConcurrentRing::is_shutdown()`.

### Sender Drop

**SpscSender::Drop**

Calls `reclaimer::shutdown_and_report(&self.ring)`, which shuts the ring down and reports all
drained items. This ensures the channel is always cleaned up when the single sender goes out
of scope.

**MpscSender::Drop**

Uses `Arc<AtomicUsize>` sender counting with the Arc::drop ordering pattern:

1. `fetch_sub(1, Release)` on the shared counter.
2. If the previous value was 1 (this is the last sender): `fence(Acquire)`, then calls
   `reclaimer::shutdown_and_report(&self.ring)`.
3. If the previous value was > 1: no action (other senders still alive).

The `Release`/`Acquire` ordering ensures all writes from other senders are visible to the
last sender before it performs shutdown.

#### Synchronous Drop Reporting

`SpscSender::drop` and the last `MpscSender::drop` synchronously call
`ConcurrentRing::shutdown()` and `reclaimer::report_shutdown(items)` on the dropping thread.
Every still-buffered item is delivered to its `shutdown_channel` inline before `Drop` returns.

`ReportChannel::send` implementations passed to Caducus must be synchronous, non-blocking, and
bounded in cost because they may execute inside `Drop`. `catch_unwind` protects the destructor
from a panicking `send`, but it does not protect against blocking I/O, slow computation, or a
channel that waits on an external condition. A blocking implementation will stall the dropping
thread for the full drain.

If the caller wants to control when reporting work happens, call `shutdown()` explicitly
before the sender goes out of scope. The drop will then drain an empty buffer and return
immediately.

See `docs/receiver.md` and `docs/reclaimer.md` for the receiver and reporting sides of this
contract.

**MpscSender::Clone**

Increments `sender_count` with `Relaxed` ordering (sufficient because clone captures channel
state under its own mutex locks) and clones the `Arc<AtomicUsize>`. Both channel locks are held
during the snapshot to match the atomicity guarantee of `set_channels`.

### Validation

- `tests/caducus.rs` covers: bounded-full behavior, receiver-closed and shutdown behavior,
  clone-local expiry and shutdown channel capture, TTL/capacity update propagation, SPSC send
  pattern, MPSC send pattern, `InvalidPattern` rejection when the wrong send variant is used,
  `NoRuntime` when no Tokio runtime is available, SPSC sender drop triggers shutdown,
  MPSC last sender drop triggers shutdown, MPSC clone drop does not trigger shutdown,
  `set_channels` atomically updates both report channels.
- Per-item expiry coverage includes SPSC and MPSC cases for `send_with_ttl`,
  `send_with_deadline`, invalid TTL item recovery, MPSC report-channel snapshot preservation,
  expiry reporting, shutdown reporting, FIFO delivery despite per-item deadline order, and default
  TTL preservation after per-item sends.
- Heavy per-item coverage includes multiple cloned MPSC senders using per-item TTL/deadline while
  preserving clone-local report-channel snapshots, reclaimer wakeup for a later-enqueued earlier
  deadline, per-item sends interleaved with capacity shrink/growth, and the conservation invariant
  under mixed default/per-item TTL workloads.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
