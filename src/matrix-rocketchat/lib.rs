//! Application service to bridge Matrix <-> Rocket.Chat.

#![feature(proc_macro)]

#![deny(missing_docs)]

#[macro_use]
extern crate error_chain;
extern crate iron;
#[macro_use]
extern crate lazy_static;
extern crate router;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_yaml;
#[macro_use]
extern crate slog;
extern crate slog_term;
extern crate yaml_rust;

/// Translations
#[macro_use]
pub mod i18n;
/// Helpers to interact with the application service configuration.
pub mod config;
/// Application service errors
pub mod errors;
/// Iron handlers
pub mod handlers;
/// The server that runs the application service.
pub mod server;

pub use config::Config;
pub use server::Server;
