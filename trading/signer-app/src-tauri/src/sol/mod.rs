//! Solana side of the unified client (Wave 4): the execution runtime
//! (sell-stream + copy-stream consumers over the shared `BotEngine`),
//! the gateway read layer backing the GUI surfaces, persisted execution
//! config (mandatory copy budget), and the keystore import commands.

pub mod commands;
pub mod config;
pub mod gateway;
pub mod runtime;
