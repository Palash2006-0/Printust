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

/// Duplex printing mode extracted from Win32 `DEVMODEW.dmDuplex`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DuplexMode {
    Simplex,
    DuplexLongEdge,
    DuplexShortEdge,
    #[default]
    Unknown,
}

impl DuplexMode {
    /// Canonical uppercase representation persisted in SQLite `print_jobs.duplex_mode`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Simplex => "SIMPLEX",
            Self::DuplexLongEdge => "DUPLEX_LONG_EDGE",
            Self::DuplexShortEdge => "DUPLEX_SHORT_EDGE",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// True only when 2-sided printing was affirmatively detected; `Unknown` falls back to simplex.
    pub fn is_duplex_billable(&self) -> bool {
        matches!(self, Self::DuplexLongEdge | Self::DuplexShortEdge)
    }

    /// Convert a known boolean flag into an explicit `DuplexMode`.
    pub fn from_bool(is_duplex: bool) -> Self {
        if is_duplex {
            Self::DuplexLongEdge
        } else {
            Self::Simplex
        }
    }
}

impl fmt::Display for DuplexMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DuplexMode {
    type Err = LedgerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "SIMPLEX" => Ok(Self::Simplex),
            "DUPLEX_LONG_EDGE" => Ok(Self::DuplexLongEdge),
            "DUPLEX_SHORT_EDGE" => Ok(Self::DuplexShortEdge),
            "UNKNOWN" => Ok(Self::Unknown),
            other => Err(LedgerError::InvalidInput(format!(
                "invalid duplex_mode: {other}"
            ))),
        }
    }
}

/// Color mode extracted from Event `805` (`RenderJobDiag/Color`) or `DEVMODEW.dmColor`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ColorMode {
    Monochrome,
    Color,
    #[default]
    Unknown,
}

impl ColorMode {
    /// Canonical uppercase representation persisted in SQLite `print_jobs.color_mode`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Monochrome => "MONOCHROME",
            Self::Color => "COLOR",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// True when color was detected; `Unknown` defaults safely to monochrome.
    pub fn is_color_billable(&self) -> bool {
        matches!(self, Self::Color)
    }

    /// Convert a known boolean flag into an explicit `ColorMode`.
    pub fn from_bool(is_color: bool) -> Self {
        if is_color {
            Self::Color
        } else {
            Self::Monochrome
        }
    }
}

impl fmt::Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ColorMode {
    type Err = LedgerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "MONOCHROME" => Ok(Self::Monochrome),
            "COLOR" => Ok(Self::Color),
            "UNKNOWN" => Ok(Self::Unknown),
            other => Err(LedgerError::InvalidInput(format!(
                "invalid color_mode: {other}"
            ))),
        }
    }
}

/// Normalized print-job payload emitted by the Windows spooler + Event Log watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingPrintJob {
    /// Windows spooler job identifier (`JOB_INFO_2.JobId` / Event 307 `Param1`).
    pub windows_job_id: u32,
    /// Optional Event Log `EventRecordID` from Event `307` for idempotent deduplication.
    #[serde(default)]
    pub event_record_id: Option<i64>,
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
    /// Whether the job used 2-sided printing (`false` when simplex or `DuplexMode::Unknown`).
    pub is_duplex: bool,
    /// Whether the job was rendered in color (`dmColor == 2` / Event 805 `Color == 2`).
    pub is_color: bool,
    /// Explicit tri-state/four-state duplex mode (`Unknown` falls back to simplex billing).
    #[serde(default)]
    pub duplex_mode: DuplexMode,
    /// Explicit color classification (`Unknown` falls back safely to monochrome).
    #[serde(default)]
    pub color_mode: ColorMode,
    /// Optional anomaly marker (e.g., `"DEVMODE_RACE_MISSED"` when `GetJobW` missed a sub-50ms job).
    pub driver_anomaly_flag: Option<String>,
    /// ISO-8601 UTC capture timestamp from Windows Event `TimeCreated/@SystemTime`.
    pub captured_at: String,
}

impl IncomingPrintJob {
    /// Effective duplex flag for billing; `DuplexMode::Unknown` safely falls back to simplex.
    pub fn resolved_is_duplex(&self) -> bool {
        match self.duplex_mode {
            DuplexMode::DuplexLongEdge | DuplexMode::DuplexShortEdge => true,
            DuplexMode::Simplex | DuplexMode::Unknown => false,
        }
    }

    /// Effective color flag for billing; honors explicit `ColorMode` or boolean fallback when `Unknown`.
    pub fn resolved_is_color(&self) -> bool {
        match self.color_mode {
            ColorMode::Color => true,
            ColorMode::Monochrome => false,
            ColorMode::Unknown => self.is_color,
        }
    }

    /// Preserve custom anomaly markers or default to `"DEVMODE_RACE_MISSED"` when duplex is `Unknown`.
    pub fn resolved_anomaly_flag(&self) -> Option<String> {
        match &self.driver_anomaly_flag {
            Some(flag) if !flag.trim().is_empty() => Some(flag.clone()),
            _ if self.duplex_mode == DuplexMode::Unknown => Some("DEVMODE_RACE_MISSED".to_owned()),
            _ => None,
        }
    }

    /// Resolve `(pages_per_copy, copies)` safely without double-multiplying unrolled Event 307 counts.
    pub fn resolved_pricing_inputs(&self) -> (u32, u32) {
        resolve_pricing_inputs(
            self.raw_pages,
            self.raw_copies,
            self.effective_pages,
            self.resolved_is_duplex(),
        )
    }
}

/// Shared unrolled-copy heuristic resolving `(pages_per_copy, copies)` from stored or live fields.
pub fn resolve_pricing_inputs(
    raw_pages: u32,
    raw_copies: u32,
    effective_pages: u32,
    is_duplex: bool,
) -> (u32, u32) {
    let copies = raw_copies.max(1);
    if raw_pages > 0 && raw_pages.saturating_mul(copies) == effective_pages {
        (raw_pages, copies)
    } else if is_duplex && copies > 1 && effective_pages.is_multiple_of(copies) {
        (effective_pages / copies, copies)
    } else {
        (effective_pages, 1)
    }
}

/// Result of an idempotent print-job ingestion attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "summary", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IngestOutcome {
    /// Event was newly inserted, billed, and persisted (with optional bookmark advance).
    Applied(Box<SessionSummary>),
    /// Event `(printer_name, event_record_id)` was already processed and skipped cleanly.
    DuplicateIgnored,
}

/// Ink category for per-page paper jam deductions in mixed B&W + Color sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum JamInkKind {
    /// Deduct 1 black-and-white jammed side (`PricingRules::bw_single_paise`).
    Bw,
    /// Deduct 1 color jammed side (`PricingRules::color_single_paise`).
    Color,
}

impl JamInkKind {
    /// Human-readable uppercase tag used in error messages and diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bw => "B&W",
            Self::Color => "COLOR",
        }
    }
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

/// Compact per-job breakdown entry included in `SessionSummary` for instant Mini-HUD clarity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionJobBreakdown {
    pub job_id: i64,
    pub doc_name: String,
    pub effective_pages: i64,
    pub duplex_mode: DuplexMode,
    pub color_mode: ColorMode,
    pub is_duplex: bool,
    pub is_color: bool,
    pub cost_paise: i64,
    pub anomaly: Option<String>,
    pub is_unverified: bool,
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
    #[serde(default)]
    pub bw_jam_deducted_pages: i64,
    #[serde(default)]
    pub color_jam_deducted_pages: i64,
    #[serde(default)]
    pub bw_jam_rate_paise: i64,
    #[serde(default)]
    pub color_jam_rate_paise: i64,
    pub addons_total_paise: i64,
    pub total_amount_paise: i64,
    pub has_anomaly: bool,
    #[serde(default)]
    pub unverified_jobs_count: i64,
    #[serde(default)]
    pub anomaly_reasons: Vec<String>,
    #[serde(default)]
    pub jobs: Vec<SessionJobBreakdown>,
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
