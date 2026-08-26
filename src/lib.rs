//! paneld — a minimal TRMNL BYOS panel server.
//!
//! The shape of the process:
//!
//! - [`config`] parses a TOML dashboard description into a validated `Config`.
//! - [`content`] stores values pushed by any device on the network, addressed by
//!   widget id, last write wins.
//! - [`render`] turns a device's config plus its content into encoded PNG frame
//!   bytes. Pure: config and content in, bytes out.
//! - [`frame`] holds the frame currently served for each device, plus exactly one
//!   generation back, because a device may be mid-download of the frame it
//!   replaced.
//! - [`schedule`] decides which devices are due for a rebuild at a given instant.
//! - [`renderer`] is the single background task that owns all rendering.
//! - [`http`] is the router. A device poll is a pure read of [`frame`]: it never
//!   renders, so poll latency is flat and independent of how expensive a
//!   dashboard is.

pub mod app;
pub mod config;
pub mod content;
pub mod frame;
pub mod ha;
pub mod http;
pub mod icon;
pub mod render;
pub mod renderer;
pub mod schedule;
pub mod sink;
pub mod status;
pub mod tap;
pub mod telemetry;
