//! Channel adapters for RouterAi.
//!
//! Core stays free of Telegram / vendor types. Adapters map wire formats ↔ [`routerai::Event`].

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod http;
pub mod webhook;

// P14: `pub mod telegram;`
