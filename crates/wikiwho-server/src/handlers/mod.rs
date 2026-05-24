//! HTTP handlers grouped by endpoint family from `API.md`.
//!
//! Currently implements:
//! - `rev_content` (§1-6) — `rev_content::*`
//! - `whocolor` (§7-8) — `whocolor::*`
//! - `health` — liveness probe (`/healthz`), not part of `API.md`.
//!
//! Not yet implemented:
//! - Ephemeral non-mainspace (§9)

pub mod health;
pub mod rev_content;
pub mod whocolor;
