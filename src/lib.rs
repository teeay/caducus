// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

//! Bounded async MPSC/SPSC channel with item expiry.
//!
//! Caducus (latin) = perishable
//! 
//! Caducus is a bounded asynchronous channel with two operating modes:
//! single-producer single-consumer ([`SpscBuilder`] / [`SpscSender`]) and
//! multi-producer single-consumer ([`MpscBuilder`] / [`MpscSender`]). Items
//! carry a time-to-live and are evicted when they expire; the eviction is
//! observable through user-supplied [`ReportChannel`]s.
//!
//! Use `shutdown()` on the sender side for controlled channel teardown. Dropping the last
//! sender also triggers a hard shutdown that drains and reports any items still in
//! the buffer.
//!
//! Caducus runs on Tokio. A runtime handle must be available either implicitly
//! (call `build()` from within a Tokio task) or explicitly via
//! [`SpscBuilder::runtime`] / [`MpscBuilder::runtime`].
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use caducus::MpscBuilder;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, rx) = MpscBuilder::<u32>::new(128, Duration::from_secs(5))
//!         .build()
//!         .expect("tokio runtime available");
//!
//!     tx.send(42).expect("buffer not full");
//!     let item = rx.next(None).await.expect("received before timeout");
//!     assert_eq!(item, 42);
//! }
//! ```
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]
#![doc = include_str!("../docs/caducus.md")]

/// Error types returned from every fallible operation in the public API.
pub mod error;
/// Receive side of the channel. See [`Receiver`].
pub mod receiver;
/// Send side of the channel. See [`SpscBuilder`], [`MpscBuilder`], [`SpscSender`],
/// and [`MpscSender`].
pub mod sender;

mod concurrency;
mod reclaimer;

pub use crate::concurrency::ReportChannel;
pub use error::{CaducusError, CaducusErrorKind};
pub use receiver::Receiver;
pub use sender::{MpscBuilder, MpscSender, SpscBuilder, SpscSender};
