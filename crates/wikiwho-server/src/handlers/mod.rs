//! HTTP handlers grouped by endpoint family from `API.md`.
//!
//! Currently implements:
//! - `rev_content` (§1-6) — `rev_content::*`
//! - `whocolor` (§7-8) — `whocolor::*`
//!
//! Not yet implemented:
//! - Ephemeral non-mainspace (§9)

pub mod rev_content;
pub mod whocolor;
