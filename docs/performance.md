# Performance Testing

Status: Developed

## Objectives

- Measure throughput across a wide range of sender/receiver speed combinations in both SPSC and
  MPSC modes.
- Identify bottlenecks under backpressure, contention, and expiry-heavy workloads.
- Produce repeatable throughput metrics (items/sec) for each scenario.

## Technical Details

### Construction And Resize Cost

Empty slots store a process-wide sentinel `Instant` (lazily initialised once per process), not
a fresh `Instant::now()` per slot. Large-capacity ring construction and resize operations
therefore avoid one clock read per empty slot. See `docs/ring-buffer.md` `### Slot`.

### Test Harness

`tests/performance.rs`, integration test.

Each scenario runs for a fixed duration or item count, measures elapsed wall time, and reports
throughput in items/sec. Results are printed to stdout (`cargo test --test performance --
--nocapture`).

Sender and receiver run as separate Tokio tasks. Timing starts after setup and ends when all items
are sent and received (or the run duration elapses). Warmup iterations are excluded from
measurement.

### Payload

A fixed-size struct (`[u8; 64]`) to keep allocation noise low and make results comparable across
scenarios.

### Speed Profiles

Speed variation is modelled with per-item delays on sender and/or receiver side.

| Profile              | Sender delay   | Receiver delay | Pressure        |
|----------------------|----------------|----------------|-----------------|
| Saturated            | None           | None           | Max throughput   |
| Fast sender          | None           | 10us--1ms      | Buffer fills up  |
| Fast receiver        | 10us--1ms      | None           | Receiver waits   |
| Matched              | 100us          | 100us          | Steady state     |
| Bursty sender        | 0 / 5ms toggle | None           | Periodic spikes  |
| Varying              | Ramp 0--500us  | Fixed 100us    | Shifting balance |

### Scenarios

#### SPSC Baseline

| #  | Scenario                        | Capacity | TTL    | Speed profile   |
|----|---------------------------------|----------|--------|-----------------|
| S1 | Saturated throughput            | 1024     | 60s    | Saturated       |
| S2 | Backpressure (fast sender)      | 64       | 60s    | Fast sender     |
| S3 | Idle receiver (fast receiver)   | 1024     | 60s    | Fast receiver   |
| S4 | Matched pacing                  | 256      | 60s    | Matched         |
| S5 | Bursty sender                   | 128      | 60s    | Bursty sender   |
| S6 | Varying sender speed            | 256      | 60s    | Varying         |
| S7 | Small buffer under pressure     | 4        | 60s    | Saturated       |
| S8 | Expiry-heavy (short TTL)        | 256      | 1ms    | Fast sender     |
| S9 | Payload baseline (64B)          | 256      | 60s    | Saturated       |

#### MPSC Contention

| #   | Scenario                        | Senders | Capacity | TTL    | Speed profile   |
|-----|---------------------------------|---------|----------|--------|-----------------|
| M1  | 2 senders saturated             | 2       | 1024     | 60s    | Saturated       |
| M2  | 4 senders saturated             | 4       | 1024     | 60s    | Saturated       |
| M3  | 8 senders saturated             | 8       | 1024     | 60s    | Saturated       |
| M4  | 16 senders saturated            | 16      | 1024     | 60s    | Saturated       |
| M5  | 4 senders, fast sender          | 4       | 64       | 60s    | Fast sender     |
| M6  | 4 senders, fast receiver        | 4       | 1024     | 60s    | Fast receiver   |
| M7  | 4 senders, bursty               | 4       | 128      | 60s    | Bursty sender   |
| M8  | 4 senders, varying speed        | 4       | 256      | 60s    | Varying         |
| M9  | 4 senders, small buffer         | 4       | 4        | 60s    | Saturated       |
| M10 | 4 senders, expiry-heavy         | 4       | 256      | 1ms    | Fast sender     |
| M11 | Mixed-speed senders             | 4       | 256      | 60s    | Per-sender vary |
| M12 | Scaling: 2/4/8/16/32 senders    | varies  | 1024     | 60s    | Saturated       |

#### Expiry And Reclaimer

| #  | Scenario                            | Mode | Capacity | TTL     | Notes                        |
|----|-------------------------------------|------|----------|---------|------------------------------|
| E1 | All items expire (no receiver)      | SPSC | 256      | 1ms     | Reclaimer-only drain         |
| E2 | 50% expiry rate                     | SPSC | 256      | Tuned   | Receiver slower than TTL     |
| E3 | Reclaimer under contention          | MPSC | 256      | 1ms     | 4 senders, short TTL         |
| E4 | TTL update mid-run                  | SPSC | 256      | 1ms→60s | Switch TTL during test       |
| E5 | Capacity resize mid-run             | SPSC | 64→512   | 60s     | Grow capacity during test    |

#### Buffer Capacity Scaling

| #  | Scenario                        | Mode | Capacity        | Speed profile |
|----|---------------------------------|------|-----------------|---------------|
| C1 | Capacity sweep (SPSC)           | SPSC | 4/16/64/256/1K  | Saturated     |
| C2 | Capacity sweep (MPSC, 4 sender) | MPSC | 4/16/64/256/1K  | Saturated     |

### Metrics

Each scenario reports:

- **Throughput**: items/sec (total items received / elapsed time).
- **Send rejections**: count of `Full` errors.
- **Expiry count**: items removed by reclaimer.
- **Shutdown count**: items drained during shutdown.
- **Elapsed time**: wall time for the run.
- **Items sent / received**: totals for sanity checking.

Output format (one line per scenario):

```
[S1]   1234567 items/sec | sent: 100000 | recv: 100000 | full: 0 | expired: 0 | shutdown: 0 | 1.234s
```

### Run Configuration

- Default item count: 100,000 total per scenario (`ITEMS`). MPSC scenarios divide equally across
  senders (`ITEMS / num_senders` per sender). Slow profiles use 10,000 total (`ITEMS_SLOW`).
- Warmup: 1,000 items excluded from timing.
- Tokio multi-threaded runtime with default worker count.
- Scenarios are independent and run sequentially to avoid interference.

### Validation

- Each scenario verifies `items_received + items_expired + items_shutdown == items_sent`
  (conservation invariant) using strict `assert_eq!`.
- Throughput values are printed, not asserted against thresholds (hardware-dependent).
- The test suite passes with `cargo test --test performance -- --nocapture`.

<!--
This file is part of the caducus crate.
SPDX-FileCopyrightText: 2026 Zivatar Limited
SPDX-License-Identifier: Apache-2.0
-->
