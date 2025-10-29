//! Solver DEX utilities for swap routing and transaction construction.
//!
//! This crate provides reusable routing logic and Uniswap V4 helpers that can be
//! shared across solver components (core engine, delivery pipeline, etc.).

pub mod router;
pub mod uniswap_v4;

pub use router::{DexError, DexRouter, SwapQuote, SwapRoute};
pub use uniswap_v4::UniswapV4Router;
