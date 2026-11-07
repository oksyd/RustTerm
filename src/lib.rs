//! RustTerm's configuration, terminal model and session services.
//!
//! The desktop application lives in the binary target. These shared modules
//! also provide the library target used by integration tests.

pub mod app_lock;
pub mod config;
pub mod connection_probe;
pub mod database;
pub mod docker;
pub mod elasticsearch;
mod http_transport;
pub mod i18n;
pub mod rdp;
pub mod redis;
pub mod session;
pub mod terminal;
pub mod update;
