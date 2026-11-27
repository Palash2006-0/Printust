//! Domain structs, enums, Serde mappings, and typed error variants for Printust.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Lifecycle state of a customer billing session stored in SQLite (`sessions.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionStatus {
    /// Currently open session accumulating incoming spooler jobs and add-ons.
    Active,
    /// Closed and paid via offline Dynamic UPI QR.
    PaidUpi,
    /// Closed and paid in cash.
    PaidCash,
    /// Closed and recorded on customer credit (Khata).
    Khata,
    /// Cancelled / voided session tracked separately in EOD leakage audits.
    Void,
}

impl SessionStatus {
    /// Canonical uppercase SQLite representation matching the `CHECK` constraint on `sessions.status`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "ACTIVE",
            Self::PaidUpi => "PAID_UPI",
            Self::PaidCash => "PAID_CASH",
            Self::Khata => "KHATA",
            Self::Void => "VOID",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = LedgerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ACTIVE" => Ok(Self::Active),
            "PAID_UPI" => Ok(Self::PaidUpi),
            "PAID_CASH" => Ok(Self::PaidCash),
            "KHATA" => Ok(Self::Khata),
            "VOID" => Ok(Self::Void),
            other => Err(LedgerError::InvalidStatus(other.to_owned())),
        }
    }
}

/// Configurable Indian print-shop rate slabs stored strictly as integer `i64` paise.
///
/// Storing currency in paise (`Rs. 1 = 100 paise`) avoids IEEE-754 floating-point drift
/// across thousands of daily print jobs and EOD SQL aggregations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingRules {
    /// Rate per 1-sided black & white page in paise (default: Rs. 2.00 = `200`).
    pub bw_single_paise: i64,
    /// Rate per 2-sided black & white physical sheet in paise (default: Rs. 3.00 = `300`).
    pub bw_duplex_sheet_paise: i64,
    /// Rate per 1-sided color page in paise (default: Rs. 10.00 = `1000`).
    pub color_single_paise: i64,
    /// Rate per 2-sided color physical sheet in paise (default: Rs. 18.00 = `1800`).
    pub color_duplex_sheet_paise: i64,
    /// Minimum total B&W sides required to trigger the bulk per-side rate (default: `50`).
    pub bw_bulk_threshold_sides: u32,
    /// Discounted rate per B&W side once `total_sides >= bw_bulk_threshold_sides` (default: Rs. 1.00 = `100`).
    pub bw_bulk_side_paise: i64,
}

impl Default for PricingRules {
    fn default() -> Self {
        Self {
            bw_single_paise: 200,
            bw_duplex_sheet_paise: 300,
            color_single_paise: 1000,
            color_duplex_sheet_paise: 1800,
            bw_bulk_threshold_sides: 50,
            bw_bulk_side_paise: 100,
        }
    }
}

impl PricingRules {
    /// Ensure all configured paise rates are non-negative so bills can never go negative.
    pub fn validate(&self) -> Result<(), LedgerError> {
        for (name, rate) in [
            ("bw_single_paise", self.bw_single_paise),
            ("bw_duplex_sheet_paise", self.bw_duplex_sheet_paise),
            ("color_single_paise", self.color_single_paise),
            ("color_duplex_sheet_paise", self.color_duplex_sheet_paise),
            ("bw_bulk_side_paise", self.bw_bulk_side_paise),
        ] {
            if rate < 0 {
                return Err(LedgerError::InvalidInput(format!(
                    "{name} must be >= 0 paise (got {rate})"
                )));
            }
        }
        Ok(())
    }
}

/// Normalized print-job payload emitted by the Windows spooler + Event Log watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingPrintJob {
    /// Windows spooler job identifier (`JOB_INFO_2.JobId` / Event 307 `Param1`).
    pub windows_job_id: u32,
    /// Target Windows printer queue name (e.g., `ShopTest_PCL6`).
    pub printer_name: String,
    /// Document title (`JOB_INFO_2.pDocument` when captured, else Event 307 `Param2`).
    pub doc_name: String,
    /// Logical page/side count reported by Event 307 or `JOB_INFO_2`.
    pub raw_pages: u32,
    /// Copy count reported by `DEVMODE.dmCopies` / Event 805 `Copies`.
    pub raw_copies: u32,
    /// Normalized effective side/sheet count after driver-lie heuristics.
    pub effective_pages: u32,
    /// Whether the job used 2-sided printing (`false` when simplex or unknown fallback).
    pub is_duplex: bool,
    /// Whether the job was rendered in color (`dmColor == 2` / Event 805 `Color == 2`).
    pub is_color: bool,
    /// Optional anomaly marker (e.g., `"DUPLEX_UNKNOWN"` when `GetJobW` missed a sub-50ms job).
    pub driver_anomaly_flag: Option<String>,
    /// ISO-8601 UTC capture timestamp from Windows Event `TimeCreated/@SystemTime`.
    pub captured_at: String,
}

/// Non-print shop add-on attached to a billing session (e.g., Spiral Binding, Lamination).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAddon {
    pub id: i64,
    pub session_id: i64,
    pub label: String,
    pub qty: i64,
    pub unit_price_paise: i64,
}

/// Live UI summary snapshot emitted to the floating Mini-HUD over Tauri IPC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: i64,
    pub status: SessionStatus,
    pub started_at: String,
    pub closed_at: Option<String>,
    pub job_count: i64,
    pub bw_pages: i64,
    pub color_pages: i64,
    pub jam_deducted_pages: i64,
    pub addons_total_paise: i64,
    pub total_amount_paise: i64,
    pub has_anomaly: bool,
    pub last_doc_name: Option<String>,
}

/// End-of-day owner reconciliation summary comparing spooler activity against billed totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyAuditReport {
    pub date_prefix: String,
    pub total_spooler_pages: i64,
    pub billed_pages: i64,
    pub jam_deducted_pages: i64,
    pub void_or_unbilled_pages: i64,
    pub upi_total_paise: i64,
    pub cash_total_paise: i64,
    pub khata_total_paise: i64,
    pub discrepancy_flag: bool,
}

/// Typed error enum propagated via `Result<T, LedgerError>` across `printust_core`.
///
/// Using explicit `Result` propagation (zero `.unwrap()`/`.expect()` in `core/src/`) ensures
/// transient SQLite or malformed input errors never panic the background watcher thread.
#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("invalid session status: {0}")]
    InvalidStatus(String),

    #[error("session {0} not found")]
    SessionNotFound(i64),

    #[error("invalid input: {0}")]
    InvalidInput(String),
}
