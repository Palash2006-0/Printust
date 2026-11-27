//! Core domain types, integer-paise pricing engine, SQLite WAL store, and session state machine for Printust.
//!
//! This crate is intentionally decoupled from Tauri, UI, and Win32 FFI so all billing
//! calculations, SQLite transactions, and audit queries remain deterministic and testable in isolation.

pub mod db;
pub mod models;
pub mod pricing;
pub mod session;

pub use db::LedgerDb;
pub use models::{
    DailyAuditReport, IncomingPrintJob, LedgerError, PricingRules, SessionAddon, SessionStatus,
    SessionSummary,
};
pub use pricing::calculate_job_cost_paise;
pub use session::SessionEngine;
