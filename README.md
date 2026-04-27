# caducus

Bounded async channel with item expiry and hard shutdown. Supports SPSC and MPSC operating modes.

Mutex-based architecture with Vec-backed ring buffer storage. A unified drain path with two
Notify handles coordinates the reclaimer task and receiver without racing. User-provided report
channels are called with unwind isolation so panicking implementations cannot kill internal tasks
or abort through destructors.

## Install

```sh
cargo add caducus
```

## Example

```rust,no_run
use std::time::Duration;
use caducus::MpscBuilder;

#[tokio::main]
async fn main() {
    let (tx, rx) = MpscBuilder::<u32>::new(128, Duration::from_secs(5))
        .build()
        .expect("tokio runtime available");

    tx.send(42).expect("buffer not full");
    let item = rx.next(None).await.expect("received before timeout");
    assert_eq!(item, 42);
}
```

## Documentation

API reference: <https://docs.rs/caducus>.

Architecture and design notes:

- `docs/caducus.md` — architecture overview, layering, public API, conservation invariant
- `docs/ring-buffer.md` — storage layer: structure, operations, resize, mode-aware push/pop
- `docs/concurrency.md` — concurrency layer: DrainMode, two Notify handles, send/drain paths
- `docs/reclaimer.md` — reclaimer task, try_receive, expiry/shutdown reporting, unwind isolation
- `docs/sender.md` — builders, send patterns, clone semantics, sender drop, set_channels
- `docs/receiver.md` — receive behaviour, deadline-based API, expired-head fallback
- `docs/performance.md` — performance test scenarios and conservation validation

## License

Licensed under the Apache License, Version 2.0
([LICENSE](LICENSE) or <https://www.apache.org/licenses/LICENSE-2.0>).

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
