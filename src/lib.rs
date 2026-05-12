//! Library surface for the crankers crate. Declares the shared modules
//! so both the main `ydelta-crankers` binary (`src/main.rs`) and any
//! one-shot helper binaries under `src/bin/` (`place_order`, etc.) link
//! against the same module tree — no duplicate compilation.

pub mod bank_registry;
pub mod chain_reader;
pub mod config;
pub mod handlers;
pub mod health_server;
pub mod marginfi_bank;
pub mod marginfi_rate;
pub mod metrics;
pub mod rpc;
pub mod signer;
pub mod swb_crank;
