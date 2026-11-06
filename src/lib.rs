//! Common utilities for building Ninots: small in-memory fake reproductions of services.
//!
//! - [`token`] generates the unique tokens a fake hands out.
//! - [`faults`] is the rule engine behind deliberate misbehaviour, generic over each fake's
//!   own target and fault types.
//! - [`operation`] and [`router`] describe the operations a fake serves and match request
//!   paths against them.
//! - [`server`] binds a port and serves an axum router until dropped, optionally over TLS
//!   ([`tls`]), and [`git`] holds the bare repositories and git's smart HTTP protocol.
//! - [`profile`], behind the `profile` feature, records the requests a client makes, to find
//!   the ones it repeats.
//! - [`state`], behind the `state` feature, is the handle a fake shares its state through,
//!   and snapshots of that state.
//! - [`spec`], behind the `build` feature, generates the operation table from an API
//!   description a build script pins.

pub mod faults;
#[cfg(feature = "git")]
pub mod git;
pub mod operation;
#[cfg(feature = "profile")]
pub mod profile;
pub mod router;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "build")]
pub mod spec;
#[cfg(feature = "state")]
pub mod state;
#[cfg(feature = "tls")]
pub mod tls;
pub mod token;
