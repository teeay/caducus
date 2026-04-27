# Caducus Bounded Buffer With Expiry

Status: Developed

## Objectives

- Provide a bounded asynchronous channel with expiry and hard shutdown semantics.
- Support two operating modes: SPSC (single producer, single consumer) and MPSC (multi producer,
  single consumer). Mode is set at build time and immutable.
- Keep the implementation split into storage, concurrency, reclaimer, sender, and receiver layers.
- Use a mutex-based architecture with Vec-backed ring buffer storage.
- Report expiry and shutdown outcomes through sender-owned report channels.

## Technical Details

### Layering

Five modules, strict separation of concerns. Each module doc is the single source of truth for
that module's behaviour.

#### Ring Buffer — Data Ownership And Integrity

`src/concurrency/ring_buffer.rs`, private submodule of the concurrency module.

The ring buffer is the single authority for all accounting data and its validity. No upper layer
computes, transforms, or second-guesses ring buffer data. Upper layers query ring state and pass
configuration values which the ring validates and applies.

See `docs/ring-buffer.md` for structure, operations, and validation.

#### Error Model

`src/error.rs`, public module.

All errors use the unified `CaducusError<T>` / `CaducusErrorKind<T>` types. The default type
parameter is `()`.

`CaducusErrorKind<T>` variants:

- `InvalidArgument` -- invalid configuration value (e.g. TTL out of range).
- `InvalidPattern(T)` -- wrong push variant for the configured mode. Carries the rejected item.
- `Timeout` -- blocking pop deadline reached.
- `Shutdown(T)` -- queue has been shut down. Carries the rejected item on the send path; carries
  `()` on the receive path.
- `Full(T)` -- queue is at capacity. Carries the rejected item.
- `NoRuntime` -- no Tokio runtime available when `build()` is called.

The send path returns `Result<(), CaducusError<T>>` so the caller always gets the item back on
failure. The receive path and configuration methods return `Result<..., CaducusError<()>>`.

`CaducusError<T>::into_inner()` returns `Option<T>`, extracting the item from `Full`, `Shutdown`,
or `InvalidPattern` variants.

#### Concurrency — Serialised Access And Wakeup Coordination

`src/concurrency.rs`, crate-private module.

Serialises access to the ring buffer via `Mutex<Ring<T>>` and coordinates wakeups via two
`Arc<Notify>` handles (one for the reclaimer task, one for the receiver). Provides a unified
`drain` method controlled by `DrainMode` for both the reclaimer and receiver paths.

See `docs/concurrency.md` for send/drain paths, error model, and notification flow.

#### Reclaimer — Expiry Task, Receiver Support, And Reporting

`src/reclaimer.rs`, crate-private module.

Owns the reclaimer task (expiry draining loop), all item reporting logic (expiry and shutdown),
and the `try_receive` function that serves the receiver. The receiver calls `try_receive` instead
of accessing the concurrency layer directly.

See `docs/reclaimer.md` for the reclaimer task loop, `try_receive`, reporting functions, and
wakeup correctness.

#### Sender — Public Send API

`src/sender.rs`, the public send-side interface.

Owns builders and sender-local report-channel configuration. Sender drop triggers shutdown
(SPSC always, MPSC on last sender via `Arc<AtomicUsize>` sender counting).

See `docs/sender.md` for builders, send patterns, clone semantics, and drop behavior.

#### Receiver — Public Receive API

`src/receiver.rs`, the public receive-side interface.

Owns the wait loop and timeout logic. Delegates to `reclaimer::try_receive` for drain-and-claim
operations. Mode-agnostic.

See `docs/receiver.md` for receive behaviour and lifecycle.

#### Visibility

Only the sender and receiver modules expose public APIs. The concurrency layer is `pub(crate)`.
The ring buffer is a private submodule of the concurrency module, invisible to sender and receiver.

#### No Panic Paths In Live Code

`expect`, `unwrap()`, `panic!`, `unreachable!`, `unimplemented!`, and `todo!` are prohibited in all
live (non-test) code. Every fallible operation must be handled through `Result`, `Option`
combinators, or pattern matching. Safe non-panicking variants such as `unwrap_or`,
`unwrap_or_else`, and `unwrap_or_default` are permitted.

### Public API Summary

Two builders produce mode-specific sender types. Both take `new(capacity, ttl)` plus optional
`expiry_channel(...)` / `shutdown_channel(...)` methods:

- `SpscBuilder<T>::build(...)` returns `(SpscSender<T>, Receiver<T>)`.
- `MpscBuilder<T>::build(...)` returns `(MpscSender<T>, Receiver<T>)`.

`SpscSender<T>` -- not cloneable, report channels (if configured) fixed at construction at ring
level. Unconfigured channels result in silent drop of the corresponding outcome.
Methods: `send(item)`, `update_capacity`, `update_ttl`, `shutdown`, `is_closed`.

`MpscSender<T>` -- cloneable, each clone captures a snapshot of the current report channels.
Methods: `send(item)`, `set_expiry_channel`, `set_shutdown_channel`, `set_channels`,
`update_capacity`, `update_ttl`, `shutdown`, `is_closed`.

`Receiver<T>` -- mode-agnostic, single consumer.
Methods: `next(deadline: Option<Instant>)`, `is_closed`.

Error variants: `InvalidArgument`, `InvalidPattern`, `NoRuntime`, `Timeout`, `Shutdown`, `Full`.
Full API detail lives in the sender and receiver docs.

### Conservation Invariant

Every item sent through the channel is accounted for exactly once:

`sent == received + expired + shutdown`

This is enforced by the architecture: `reclaimer::try_receive` reports expired items on the
receiver path, the reclaimer task reports expired items on the drain path, and shutdown drains
report through shutdown channels. Performance tests validate strict equality.

### Validation

- `tests/ring_buffer.rs` validates storage invariants and resize behavior.
- `tests/concurrency.rs` validates drain, shutdown, and notification behavior.
- `tests/caducus.rs` validates the public sender and receiver contract.
- `tests/performance.rs` validates throughput, conservation, and stress scenarios.
- Verification passes with:
  `cargo test --test caducus`
  `cargo test`
  and `cargo clippy --all-targets --all-features -- -D warnings`.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
