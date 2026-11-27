//! High-level session state machine facade for Tauri IPC commands and the Day 8 watcher.

use crate::db::LedgerDb;
use crate::models::{
    DailyAuditReport, IncomingPrintJob, LedgerError, SessionStatus, SessionSummary,
};
use crate::pricing::calculate_job_cost_paise;

/// Resolve `(pages_per_copy, copies)` safely from an `IncomingPrintJob` without double-multiplying.
///
/// - When `GetJobW` captured per-copy pages (`raw_pages * raw_copies == effective_pages`),
///   returns `(raw_pages, raw_copies)` so multi-copy odd-duplex computes sheets per copy.
/// - When `GetJobW` missed a fast job and only unrolled Event `307` `PagesPrinted` was available
///   (so `effective_pages` already equals total impressions), returns `(effective_pages, 1)`
///   to prevent double-multiplying by `raw_copies`.
fn resolve_pricing_inputs(job: &IncomingPrintJob) -> (u32, u32) {
    let copies = job.raw_copies.max(1);
    if job.raw_pages > 0 && job.raw_pages.saturating_mul(copies) == job.effective_pages {
        (job.raw_pages, copies)
    } else if job.is_duplex && copies > 1 && job.effective_pages.is_multiple_of(copies) {
        (job.effective_pages / copies, copies)
    } else {
        (job.effective_pages, 1)
    }
}

/// High-level state machine managing the active customer billing session in SQLite.
pub struct SessionEngine {
    db: LedgerDb,
}

impl SessionEngine {
    /// Wrap an initialized `LedgerDb` handle inside a `SessionEngine`.
    pub fn new(db: LedgerDb) -> Self {
        Self { db }
    }

    /// Borrow the underlying `LedgerDb` for read queries.
    pub fn db(&self) -> &LedgerDb {
        &self.db
    }

    /// Mutably borrow the underlying `LedgerDb` (e.g., to update pricing rules).
    pub fn db_mut(&mut self) -> &mut LedgerDb {
        &mut self.db
    }

    /// Ingest a captured print job: auto-open an `ACTIVE` session if needed, compute cost
    /// in integer paise, persist atomically, and return the refreshed `SessionSummary`.
    pub fn ingest_print_job(&mut self, job: IncomingPrintJob) -> Result<SessionSummary, LedgerError> {
        let session_id = self.db.get_or_create_active_session(&job.captured_at)?;
        let rules = self.db.load_pricing_rules()?;
        let (pages_per_copy, copies) = resolve_pricing_inputs(&job);
        let job_cost_paise = calculate_job_cost_paise(
            pages_per_copy,
            copies,
            job.is_color,
            job.is_duplex,
            &rules,
        );
        self.db.append_print_job(session_id, &job, job_cost_paise)?;
        self.db.get_session_summary(session_id)
    }

    /// Deduct 1 jammed/misprinted page (`rules.bw_single_paise`) from the active session.
    pub fn apply_jam_deduction(&mut self) -> Result<Option<SessionSummary>, LedgerError> {
        let Some(session_id) = self.db.get_active_session_id()? else {
            return Ok(None);
        };
        let rules = self.db.load_pricing_rules()?;
        self.db.deduct_jam_page(session_id, rules.bw_single_paise)?;
        Ok(Some(self.db.get_session_summary(session_id)?))
    }

    /// Attach a shop add-on (e.g., Spiral Binding, Lamination) to the active session.
    pub fn add_addon(
        &mut self,
        label: &str,
        qty: i64,
        unit_price_paise: i64,
    ) -> Result<Option<SessionSummary>, LedgerError> {
        let Some(session_id) = self.db.get_active_session_id()? else {
            return Ok(None);
        };
        self.db
            .add_session_addon(session_id, label, qty, unit_price_paise)?;
        Ok(Some(self.db.get_session_summary(session_id)?))
    }

    /// Close the currently active session with a final payment/void status.
    pub fn close_active_session(
        &mut self,
        status: SessionStatus,
        now_iso: &str,
    ) -> Result<Option<SessionSummary>, LedgerError> {
        if status == SessionStatus::Active {
            return Err(LedgerError::InvalidInput(
                "cannot close active session with status ACTIVE".to_owned(),
            ));
        }
        let Some(session_id) = self.db.get_active_session_id()? else {
            return Ok(None);
        };
        self.db.close_session(session_id, status, now_iso)?;
        Ok(Some(self.db.get_session_summary(session_id)?))
    }

    /// Fetch the live `SessionSummary` of the currently active session, if any.
    pub fn get_active_summary(&self) -> Result<Option<SessionSummary>, LedgerError> {
        let Some(session_id) = self.db.get_active_session_id()? else {
            return Ok(None);
        };
        Ok(Some(self.db.get_session_summary(session_id)?))
    }

    /// Compute the end-of-day reconciliation report for `date_prefix` (e.g. `"2026-11-27"`).
    pub fn get_daily_audit(&self, date_prefix: &str) -> Result<DailyAuditReport, LedgerError> {
        self.db.get_daily_audit_report(date_prefix)
    }
}
