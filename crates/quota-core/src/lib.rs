//! Shared types, snapshot schema, protocol, and math for `quota` / `quotad`.
//!
//! Memory discipline: this crate is allocation-aware and dependency-light.
//! Snapshots are small structs. History lives in a bounded ring buffer owned
//! by the daemon — never here as a global.

#![forbid(unsafe_code)]

pub mod config;
pub mod framing;
pub mod math;
pub mod paths;
pub mod protocol;
pub mod timeutil;
pub mod types;

pub use config::Config;
pub use framing::{decode_len, encode_frame, FrameError, MAX_FRAME_BYTES};
pub use math::{burn_percent_per_sec, can_start, eta_empty_secs, select_samples};
pub use paths::{
    claude_config_dirs, codex_home, default_config_path, default_data_dir, default_socket_path,
    default_state_dir, home_dir,
};
pub use protocol::{
    CanStartParams, ErrorBody, PaceParams, ProviderFilter, Request, Response, StatusParams,
    WatchParams, METHOD_CAN_START, METHOD_PACE, METHOD_PING, METHOD_STATUS, METHOD_VERSION,
    METHOD_WATCH, PROTOCOL_VERSION,
};
pub use types::{
    AdapterError, Availability, CanStartAnswer, CanStartBasis, Credits, PaceReport, ProviderId,
    ProviderSnapshot, Snapshot, Source, UsageWindow, WindowKind, PACKAGE_VERSION,
};
