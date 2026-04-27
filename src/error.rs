// This file is part of the caducus crate.
// SPDX-FileCopyrightText: 2026 Zivatar Limited
// SPDX-License-Identifier: Apache-2.0

/// Error kinds for the public Caducus API.
///
/// The default type parameter `()` is used by non-send error paths (receive,
/// config, construction). The send path uses `CaducusErrorKind<T>` so that
/// `Full` and `Shutdown` carry the rejected item back to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaducusErrorKind<T = ()> {
    /// A configuration value was outside the supported range (for example a TTL
    /// of zero or capacity of zero).
    InvalidArgument,
    /// The wrong send variant was used for the configured channel mode. Carries
    /// the rejected item.
    InvalidPattern(T),
    /// `build()` was called without a Tokio runtime available.
    NoRuntime,
    /// A receive operation reached its deadline without producing an item.
    Timeout,
    /// The channel has been shut down. On the send path this carries the
    /// rejected item; on the receive path the parameter is `()`.
    Shutdown(T),
    /// The buffer is at capacity. Carries the rejected item.
    Full(T),
}

/// Public error type wrapping a [`CaducusErrorKind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaducusError<T = ()> {
    /// The error kind, carrying the rejected item where applicable.
    pub kind: CaducusErrorKind<T>,
}

impl<T> std::fmt::Display for CaducusError<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            CaducusErrorKind::InvalidArgument => write!(f, "invalid argument"),
            CaducusErrorKind::InvalidPattern(_) => write!(f, "invalid pattern"),
            CaducusErrorKind::NoRuntime => write!(f, "no tokio runtime available"),
            CaducusErrorKind::Timeout => write!(f, "timeout"),
            CaducusErrorKind::Shutdown(_) => write!(f, "shutdown"),
            CaducusErrorKind::Full(_) => write!(f, "full"),
        }
    }
}

impl<T: std::fmt::Debug> std::error::Error for CaducusError<T> {}

impl<T> CaducusError<T> {
    /// Extracts the rejected item from a `Full` or `Shutdown` error.
    /// Returns `None` for error kinds that do not carry an item.
    ///
    /// # Examples
    ///
    /// ```
    /// use caducus::{CaducusError, CaducusErrorKind};
    ///
    /// let err = CaducusError { kind: CaducusErrorKind::Full(42_u32) };
    /// assert_eq!(err.into_inner(), Some(42));
    ///
    /// let err: CaducusError<u32> = CaducusError { kind: CaducusErrorKind::NoRuntime };
    /// assert_eq!(err.into_inner(), None);
    /// ```
    pub fn into_inner(self) -> Option<T> {
        match self.kind {
            CaducusErrorKind::Full(item)
            | CaducusErrorKind::Shutdown(item)
            | CaducusErrorKind::InvalidPattern(item) => Some(item),
            _ => None,
        }
    }
}
