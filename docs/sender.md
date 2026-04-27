# Sender Capability

Status: Developed

## Objectives

- Define sender behavior for the bounded asynchronous channel.
- Support two distinct sender types: `SpscSender<T>` (not cloneable) and `MpscSender<T>`
  (cloneable with channel snapshot semantics).

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
initial sender-local report channels. These become the default channels on the first
`MpscSender<T>` and are copied to each subsequent clone.

The SPSC vs MPSC difference is now only "ring-level handle vs sender-local handle", not
"required vs optional".

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
3. Ring buffer: validates mode is SPSC (otherwise `InvalidPattern(item)`), then `push_common`.
4. `push_common`: rejects if shutdown (`Shutdown(item)`) or full (`Full(item)`). Computes
   `expires_at = now + ttl`. Writes item, expiry, and `None` for both per-slot channel fields into
   the slot at `tail`. Advances `tail`, increments `len`.
5. Concurrency layer: releases mutex, notifies both reclaimer and receiver.
6. Returns `Ok(())` to the caller. On any error, the item is returned inside the error variant.

**`MpscSender::send(item) -> Result<(), CaducusError<T>>`**

1. Snapshots both sender-local channels atomically (both locks held, then released).
2. Calls `ConcurrentRing::send_mpsc(item, expiry_channel, shutdown_channel)`.
3. Concurrency layer: acquires mutex, calls `Ring::try_push_mpsc(item, expiry_channel,
   shutdown_channel)`.
4. Ring buffer: validates mode is MPSC (otherwise `InvalidPattern(item)`), then `push_common`.
5. `push_common`: same checks as SPSC. Writes item, expiry, and the provided per-slot channel
   handles into the slot at `tail`. Advances `tail`, increments `len`.
6. Concurrency layer: releases mutex, notifies both reclaimer and receiver.
7. Returns `Ok(())` to the caller. On any error, the item is returned inside the error variant.

### Report Channel Configuration

`set_expiry_channel(ch)` and `set_shutdown_channel(ch)` update a single channel each.
`set_channels(expiry, shutdown)` updates both atomically (both locks held), preventing a
concurrent `send` from observing one old and one new channel.

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

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
