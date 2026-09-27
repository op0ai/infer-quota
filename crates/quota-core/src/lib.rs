//! Shared types, snapshot schema, protocol, and math for `quota` / `quotad`.
//!
//! Memory discipline: this crate is allocation-aware and dependency-light.
//! Snapshots are small structs. History lives in a bounded ring buffer owned
//! by the daemon — never here as a global.

#![forbid(unsafe_code)]

pub mod accounts;
pub mod config;
pub mod framing;
pub mod fsutil;
pub mod math;
pub mod paths;
pub mod protocol;
#[cfg(unix)]
pub mod rpc;
pub mod timeutil;
pub mod types;
pub mod windows;

pub use accounts::{AccountBook, AccountRecord, SecretRef};
pub use config::Config;
pub use framing::{decode_len, encode_frame, FrameError, MAX_FRAME_BYTES};
pub use fsutil::{
    chmod_private_file, create_private_file, ensure_private_dir, open_private_append,
    open_regular_file, path_has_parent_dir, read_file_capped, CapReadError,
};
pub use math::{burn_percent_per_sec, can_start, eta_empty_secs, select_samples};
pub use paths::{
    claude_config_dirs, codex_home, codexbar_history_candidates, codexbar_snapshot_candidates,
    default_codexbar_dir, default_config_path, default_data_dir, default_socket_path,
    default_state_dir, home_dir,
};
pub use protocol::{
    AccountMutationResult, AccountsAddParams, AccountsListResult, AccountsRemoveParams,
    AccountsSelectParams, CanStartParams, ErrorBody, PaceParams, ProviderFilter, RefreshParams,
    Request, Response, StatusParams, WatchParams, METHOD_ACCOUNTS_ADD, METHOD_ACCOUNTS_LIST,
    METHOD_ACCOUNTS_REMOVE, METHOD_ACCOUNTS_SELECT, METHOD_CAN_START, METHOD_PACE, METHOD_PING,
    METHOD_REFRESH, METHOD_STATUS, METHOD_VERSION, METHOD_WATCH, PROTOCOL_VERSION,
};
pub use types::{
    AdapterError, Availability, CanStartAnswer, CanStartBasis, Credits, PaceReport, ProviderId,
    ProviderSnapshot, Snapshot, Source, UsageWindow, WindowKind, PACKAGE_VERSION,
};
pub use windows::{
    classify_codex_window, FIVE_HOUR_MINUTES, FIVE_HOUR_SECS, WEEK_MINUTES, WEEK_SECS,
};
