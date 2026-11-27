//! High-level session state machine facade for Tauri IPC commands and the Day 8 watcher.

use crate::db::LedgerDb;
use crate::models::{
    DailyAuditReport, IncomingPrintJob, IngestOutcome, LedgerError, SessionStatus, SessionSummary,
};
use crate::pricing::calculate_job_cost_paise;

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

    fn compute_job_cost(&self, job: &IncomingPrintJob) -> Result<i64, LedgerError> {
        let rules = self.db.load_pricing_rules()?;
        let (pages_per_copy, copies) = job.resolved_pricing_inputs();
        Ok(calculate_job_cost_paise(
            pages_per_copy,
            copies,
            job.resolved_is_color(),
            job.resolved_is_duplex(),
            &rules,
        ))
    }

    /// Ingest a captured print job: auto-open an `ACTIVE` session if needed, compute cost
    /// in integer paise, persist atomically, and return the refreshed `SessionSummary`.
    pub fn ingest_print_job(&mut self, job: IncomingPrintJob) -> Result<SessionSummary, LedgerError> {
        let session_id = self.db.get_or_create_active_session(&job.captured_at)?;
        let job_cost_paise = self.compute_job_cost(&job)?;
        self.db.append_print_job(session_id, &job, job_cost_paise)?;
        self.db.get_session_summary(session_id)
    }

    /// Idempotently ingest a print job without a bookmark update.
    pub fn ingest_print_job_idempotent(
        &mut self,
        job: IncomingPrintJob,
    ) -> Result<IngestOutcome, LedgerError> {
        let job_cost_paise = self.compute_job_cost(&job)?;
        match self
            .db
            .ingest_job_and_bookmark_atomic(&job, job_cost_paise, None)?
        {
            Some((session_id, _)) => {
                let summary = self.db.get_session_summary(session_id)?;
                Ok(IngestOutcome::Applied(summary))
            }
            None => Ok(IngestOutcome::DuplicateIgnored),
        }
    }

    /// Idempotently ingest a print job and persist its updated Windows Event Log XML bookmark
    /// inside a **single atomic SQLite transaction**.
    pub fn ingest_print_job_with_bookmark(
        &mut self,
        job: IncomingPrintJob,
        channel_name: &str,
        bookmark_xml: &str,
    ) -> Result<IngestOutcome, LedgerError> {
        let job_cost_paise = self.compute_job_cost(&job)?;
        match self.db.ingest_job_and_bookmark_atomic(
            &job,
            job_cost_paise,
            Some((channel_name, bookmark_xml)),
        )? {
            Some((session_id, _)) => {
                let summary = self.db.get_session_summary(session_id)?;
                Ok(IngestOutcome::Applied(summary))
            }
            None => Ok(IngestOutcome::DuplicateIgnored),
        }
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
