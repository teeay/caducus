# Caducus

_Caducus (lat.): perishable_

Bounded async channel with item expiry. Supports SPSC and MPSC operating modes. Requires Tokio.

Author intended use case is for deterministic memory footprint systems with set timeouts and error handling at each intelligent module. This module is not trying to be intelligent, it just offers the receiver the data until it expires and gives it back to the sender if it is spoilt.

The author hopes others find additional use cases.

- Aims for high throughput
- Expired items drained on expiry (not on next receiver pop)
- Sender is responsible for handling returned items (no expiry/shutdown retries)
- Aims to be panick free and panick resistant

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

## Layers

- Sender/Receiver public APIs
- Reclaimer expiry tracker
- Concurrency layer
- Ring buffer layer

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

## Use of Artificial Intelligence

[OpenAI Codex](https://openai.com/codex/) and [Anthopic Claude Code](https://claude.com/product/claude-code) were used as coding agents and code reviewers.

[Google Gemini CLI](https://geminicli.com) was used as a code reviewer.

## License

Licensed under the Apache License, Version 2.0
([LICENSE](LICENSE) or <https://www.apache.org/licenses/LICENSE-2.0>).

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
