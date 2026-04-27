# Documentation

This directory contains the Caravaggio documentation set for the caducus crate. Each module
document is the single source of truth for that module's behaviour.

## Structure

- `caducus.md` — architecture overview, layering, public API, conservation invariant
- `ring-buffer.md` — storage layer: structure, operations, resize, mode-aware push/pop
- `concurrency.md` — concurrency layer: DrainMode, two Notify handles, send/drain paths
- `reclaimer.md` — reclaimer task, try_receive, expiry/shutdown reporting, unwind isolation
- `sender.md` — builders, send patterns, clone semantics, sender drop, set_channels
- `receiver.md` — receive behaviour, deadline-based API, expired-head fallback
- `performance.md` — performance test scenarios and conservation validation
- `review-memory.md` — standing list of patterns past reviews have rejected as not actionable; check before filing review items

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
