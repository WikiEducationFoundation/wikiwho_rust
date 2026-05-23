//! HTTP handlers grouped by endpoint family from `API.md`.
//!
//! Currently implements:
//! - `rev_content` (§1-6) — `rev_content::*`
//!
//! Not yet implemented:
//! - `whocolor` (§7-8)
//! - Ephemeral non-mainspace (§9)

pub mod rev_content;
