//! SQLite WAL persistence layer, append-only `print_jobs` audit trail, and EOD reconciliation.

use std::path::Path;
use std::str::FromStr;

use rusqlite::{params, Connection, OptionalExtension};

use crate::models::{
    DailyAuditReport, IncomingPrintJob, LedgerError, PricingRules, SessionStatus, SessionSummary,
};

const RULE_BW_SINGLE: &str = "BW_SINGLE";
const RULE_BW_DUPLEX_SHEET: &str = "BW_DUPLEX_SHEET";
const RULE_COLOR_SINGLE: &str = "COLOR_SINGLE";
const RULE_COLOR_DUPLEX_SHEET: &str = "COLOR_DUPLEX_SHEET";
const RULE_BW_BULK_THRESHOLD: &str = "BW_BULK_THRESHOLD_SIDES";
const RULE_BW_BULK_SIDE: &str = "BW_BULK_SIDE";

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    started_at TEXT NOT NULL,
    closed_at TEXT,
    status TEXT NOT NULL CHECK(status IN ('ACTIVE','PAID_UPI','PAID_CASH','KHATA','VOID')),
    total_amount_paise INTEGER NOT NULL DEFAULT 0,
    jam_deducted_pages INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS print_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER REFERENCES sessions(id),
    windows_job_id INTEGER NOT NULL,
    printer_name TEXT NOT NULL,
    doc_name TEXT NOT NULL,
    raw_pages INTEGER NOT NULL,
    raw_copies INTEGER NOT NULL DEFAULT 1,
    effective_pages INTEGER NOT NULL,
    is_duplex INTEGER NOT NULL DEFAULT 0,
    is_color INTEGER NOT NULL DEFAULT 0,
    driver_anomaly_flag TEXT,
    captured_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_addons (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL REFERENCES sessions(id),
    label TEXT NOT NULL,
    qty INTEGER NOT NULL DEFAULT 1,
    unit_price_paise INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS pricing_rules (
    rule_key TEXT PRIMARY KEY,
    rate_paise INTEGER NOT NULL CHECK (rate_paise >= 0)
);

CREATE INDEX IF NOT EXISTS idx_sessions_started_at ON sessions(started_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_single_active
ON sessions(status) WHERE status = 'ACTIVE';
CREATE INDEX IF NOT EXISTS idx_print_jobs_session_id ON print_jobs(session_id);
CREATE INDEX IF NOT EXISTS idx_print_jobs_captured_at ON print_jobs(captured_at);
"#;

/// SQLite-backed Printust ledger enforcing WAL durability, foreign keys, and atomic writes.
pub struct LedgerDb {
    conn: Connection,
}

impl LedgerDb {
    /// Open an isolated in-memory SQLite ledger (primarily for fast unit/integration tests).
    pub fn open_in_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        Self::init_connection(conn)
    }

    /// Open (or create) a file-backed SQLite ledger with WAL mode enabled.
    pub fn open_file(path: &Path) -> Result<Self, LedgerError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| LedgerError::InvalidInput(err.to_string()))?;
            }
        }
        let conn = Connection::open(path)?;
        Self::init_connection(conn)
    }

    fn init_connection(conn: Connection) -> Result<Self, LedgerError> {
        // Apply PRAGMAs immediately on open so WAL, FK enforcement, and busy locks are active.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )?;
        conn.execute_batch(SCHEMA_SQL)?;

        let db = Self { conn };
        db.seed_default_pricing_rules()?;
        Ok(db)
    }

    fn seed_default_pricing_rules(&self) -> Result<(), LedgerError> {
        let defaults = PricingRules::default();
        let rows = [
            (RULE_BW_SINGLE, defaults.bw_single_paise),
            (RULE_BW_DUPLEX_SHEET, defaults.bw_duplex_sheet_paise),
            (RULE_COLOR_SINGLE, defaults.color_single_paise),
            (RULE_COLOR_DUPLEX_SHEET, defaults.color_duplex_sheet_paise),
            (
                RULE_BW_BULK_THRESHOLD,
                i64::from(defaults.bw_bulk_threshold_sides),
            ),
            (RULE_BW_BULK_SIDE, defaults.bw_bulk_side_paise),
        ];

        for (key, value) in rows {
            self.conn.execute(
                "INSERT OR IGNORE INTO pricing_rules (rule_key, rate_paise) VALUES (?1, ?2)",
                params![key, value],
            )?;
        }
        Ok(())
    }

    /// Expose raw connection for read-only inspection (e.g., verifying PRAGMAs in tests).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Load configured pricing slabs from SQLite, falling back to defaults for missing keys.
    pub fn load_pricing_rules(&self) -> Result<PricingRules, LedgerError> {
        let mut rules = PricingRules::default();
        let mut stmt = self
            .conn
            .prepare("SELECT rule_key, rate_paise FROM pricing_rules")?;
        let mut rows = stmt.query([])?;

        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let value: i64 = row.get(1)?;
            if value < 0 {
                return Err(LedgerError::InvalidInput(format!(
                    "negative rate_paise ({value}) stored for {key}"
                )));
            }
            match key.as_str() {
                RULE_BW_SINGLE => rules.bw_single_paise = value,
                RULE_BW_DUPLEX_SHEET => rules.bw_duplex_sheet_paise = value,
                RULE_COLOR_SINGLE => rules.color_single_paise = value,
                RULE_COLOR_DUPLEX_SHEET => rules.color_duplex_sheet_paise = value,
                RULE_BW_BULK_THRESHOLD => {
                    rules.bw_bulk_threshold_sides = u32::try_from(value).map_err(|_| {
                        LedgerError::InvalidInput(format!(
                            "bw_bulk_threshold_sides out of u32 range: {value}"
                        ))
                    })?;
                }
                RULE_BW_BULK_SIDE => rules.bw_bulk_side_paise = value,
                _ => {}
            }
        }
        Ok(rules)
    }

    /// Validate and persist all pricing rules atomically.
    pub fn save_pricing_rules(&mut self, rules: &PricingRules) -> Result<(), LedgerError> {
        rules.validate()?;
        let tx = self.conn.transaction()?;
        let rows = [
            (RULE_BW_SINGLE, rules.bw_single_paise),
            (RULE_BW_DUPLEX_SHEET, rules.bw_duplex_sheet_paise),
            (RULE_COLOR_SINGLE, rules.color_single_paise),
            (RULE_COLOR_DUPLEX_SHEET, rules.color_duplex_sheet_paise),
            (
                RULE_BW_BULK_THRESHOLD,
                i64::from(rules.bw_bulk_threshold_sides),
            ),
            (RULE_BW_BULK_SIDE, rules.bw_bulk_side_paise),
        ];

        for (key, value) in rows {
            tx.execute(
                "INSERT INTO pricing_rules (rule_key, rate_paise)
                 VALUES (?1, ?2)
                 ON CONFLICT(rule_key) DO UPDATE SET rate_paise = excluded.rate_paise",
                params![key, value],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Return the currently open `ACTIVE` session id, if one exists.
    pub fn get_active_session_id(&self) -> Result<Option<i64>, LedgerError> {
        let id = self
            .conn
            .query_row(
                "SELECT id FROM sessions WHERE status = 'ACTIVE' ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Return the current `ACTIVE` session id, or open a fresh `ACTIVE` session.
    pub fn get_or_create_active_session(&self, now_iso: &str) -> Result<i64, LedgerError> {
        if let Some(existing_id) = self.get_active_session_id()? {
            return Ok(existing_id);
        }
        self.conn.execute(
            "INSERT INTO sessions (started_at, status, total_amount_paise, jam_deducted_pages)
             VALUES (?1, 'ACTIVE', 0, 0)",
            params![now_iso],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Ensure the target session exists and is currently `ACTIVE` inside a transaction.
    fn ensure_session_active(
        tx: &rusqlite::Transaction<'_>,
        session_id: i64,
    ) -> Result<(), LedgerError> {
        let status_str: Option<String> = tx
            .query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;

        match status_str {
            None => Err(LedgerError::SessionNotFound(session_id)),
            Some(s) if s == SessionStatus::Active.as_str() => Ok(()),
            Some(other) => Err(LedgerError::InvalidInput(format!(
                "session {session_id} is {other}, expected ACTIVE"
            ))),
        }
    }

    /// Atomically append an immutable `print_jobs` audit row and add its cost to the session.
    pub fn append_print_job(
        &mut self,
        session_id: i64,
        job: &IncomingPrintJob,
        job_cost_paise: i64,
    ) -> Result<i64, LedgerError> {
        if job_cost_paise < 0 {
            return Err(LedgerError::InvalidInput(format!(
                "job_cost_paise cannot be negative (got {job_cost_paise})"
            )));
        }
        let tx = self.conn.transaction()?;
        Self::ensure_session_active(&tx, session_id)?;

        tx.execute(
            "INSERT INTO print_jobs (
                session_id, windows_job_id, printer_name, doc_name,
                raw_pages, raw_copies, effective_pages, is_duplex, is_color,
                driver_anomaly_flag, captured_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                session_id,
                i64::from(job.windows_job_id),
                job.printer_name,
                job.doc_name,
                i64::from(job.raw_pages),
                i64::from(job.raw_copies),
                i64::from(job.effective_pages),
                i64::from(job.is_duplex),
                i64::from(job.is_color),
                job.driver_anomaly_flag,
                job.captured_at,
            ],
        )?;
        let inserted_id = tx.last_insert_rowid();

        tx.execute(
            "UPDATE sessions
             SET total_amount_paise = total_amount_paise + ?1
             WHERE id = ?2",
            params![job_cost_paise, session_id],
        )?;

        tx.commit()?;
        Ok(inserted_id)
    }

    /// Atomically record a non-print add-on (e.g., Spiral Binding, Lamination) on an active session.
    pub fn add_session_addon(
        &mut self,
        session_id: i64,
        label: &str,
        qty: i64,
        unit_price_paise: i64,
    ) -> Result<i64, LedgerError> {
        if label.trim().is_empty() {
            return Err(LedgerError::InvalidInput(
                "addon label cannot be empty".to_owned(),
            ));
        }
        if qty <= 0 {
            return Err(LedgerError::InvalidInput(format!(
                "addon qty must be > 0 (got {qty})"
            )));
        }
        if unit_price_paise < 0 {
            return Err(LedgerError::InvalidInput(format!(
                "addon unit_price_paise cannot be negative (got {unit_price_paise})"
            )));
        }

        let addon_cost_paise = qty.checked_mul(unit_price_paise).ok_or_else(|| {
            LedgerError::InvalidInput("addon total cost overflows i64 paise".to_owned())
        })?;

        let tx = self.conn.transaction()?;
        Self::ensure_session_active(&tx, session_id)?;

        tx.execute(
            "INSERT INTO session_addons (session_id, label, qty, unit_price_paise)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, label.trim(), qty, unit_price_paise],
        )?;
        let inserted_id = tx.last_insert_rowid();

        tx.execute(
            "UPDATE sessions
             SET total_amount_paise = total_amount_paise + ?1
             WHERE id = ?2",
            params![addon_cost_paise, session_id],
        )?;

        tx.commit()?;
        Ok(inserted_id)
    }

    /// Deduct 1 jammed/misprinted page from an active session, clamping `total_amount_paise >= 0`.
    pub fn deduct_jam_page(
        &mut self,
        session_id: i64,
        unit_deduct_paise: i64,
    ) -> Result<(), LedgerError> {
        if unit_deduct_paise < 0 {
            return Err(LedgerError::InvalidInput(format!(
                "unit_deduct_paise cannot be negative (got {unit_deduct_paise})"
            )));
        }

        let tx = self.conn.transaction()?;
        Self::ensure_session_active(&tx, session_id)?;

        tx.execute(
            "UPDATE sessions
             SET jam_deducted_pages = jam_deducted_pages + 1,
                 total_amount_paise = MAX(0, total_amount_paise - ?1)
             WHERE id = ?2",
            params![unit_deduct_paise, session_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Finalize an `ACTIVE` session as `PAID_UPI`, `PAID_CASH`, `KHATA`, or `VOID`.
    pub fn close_session(
        &mut self,
        session_id: i64,
        status: SessionStatus,
        closed_at: &str,
    ) -> Result<(), LedgerError> {
        if status == SessionStatus::Active {
            return Err(LedgerError::InvalidInput(
                "cannot close a session with status ACTIVE".to_owned(),
            ));
        }

        let tx = self.conn.transaction()?;
        Self::ensure_session_active(&tx, session_id)?;

        tx.execute(
            "UPDATE sessions SET status = ?1, closed_at = ?2 WHERE id = ?3",
            params![status.as_str(), closed_at, session_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Build a live `SessionSummary` snapshot for the floating Mini-HUD.
    pub fn get_session_summary(&self, session_id: i64) -> Result<SessionSummary, LedgerError> {
        let session_row: Option<(String, Option<String>, String, i64, i64)> = self
            .conn
            .query_row(
                "SELECT started_at, closed_at, status, total_amount_paise, jam_deducted_pages
                 FROM sessions WHERE id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;

        let (started_at, closed_at, status_str, total_amount_paise, jam_deducted_pages) =
            session_row.ok_or(LedgerError::SessionNotFound(session_id))?;
        let status = SessionStatus::from_str(&status_str)?;

        let (job_count, bw_pages, color_pages, anomaly_int): (i64, i64, i64, i64) =
            self.conn.query_row(
                "SELECT
                    COUNT(*),
                    COALESCE(SUM(CASE WHEN is_color = 0 THEN effective_pages ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN is_color != 0 THEN effective_pages ELSE 0 END), 0),
                    COALESCE(MAX(CASE WHEN driver_anomaly_flag IS NOT NULL AND TRIM(driver_anomaly_flag) != '' THEN 1 ELSE 0 END), 0)
                 FROM print_jobs
                 WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;

        let last_doc_name: Option<String> = self
            .conn
            .query_row(
                "SELECT doc_name FROM print_jobs WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;

        let addons_total_paise: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(qty * unit_price_paise), 0) FROM session_addons WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;

        Ok(SessionSummary {
            session_id,
            status,
            started_at,
            closed_at,
            job_count,
            bw_pages,
            color_pages,
            jam_deducted_pages,
            addons_total_paise,
            total_amount_paise,
            has_anomaly: anomaly_int != 0,
            last_doc_name,
        })
    }

    /// Reconcile captured OS spooler pages vs billed pages, jam deductions, and VOID leakage.
    pub fn get_daily_audit_report(&self, date_prefix: &str) -> Result<DailyAuditReport, LedgerError> {
        if date_prefix.trim().is_empty() {
            return Err(LedgerError::InvalidInput(
                "date_prefix cannot be empty".to_owned(),
            ));
        }

        let total_spooler_pages: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(effective_pages), 0)
             FROM print_jobs
             WHERE captured_at >= ?1 AND captured_at < (?1 || char(127))",
            params![date_prefix],
            |row| row.get(0),
        )?;

        let (upi_total_paise, cash_total_paise, khata_total_paise, jam_deducted_pages): (
            i64,
            i64,
            i64,
            i64,
        ) = self.conn.query_row(
            "SELECT
                COALESCE(SUM(CASE WHEN status = 'PAID_UPI' THEN total_amount_paise ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'PAID_CASH' THEN total_amount_paise ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status = 'KHATA' THEN total_amount_paise ELSE 0 END), 0),
                COALESCE(SUM(jam_deducted_pages), 0)
             FROM sessions
             WHERE started_at >= ?1 AND started_at < (?1 || char(127))",
            params![date_prefix],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;

        let paid_job_pages: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(pj.effective_pages), 0)
             FROM print_jobs pj
             INNER JOIN sessions s ON pj.session_id = s.id
             WHERE pj.captured_at >= ?1 AND pj.captured_at < (?1 || char(127))
               AND s.status IN ('PAID_UPI', 'PAID_CASH', 'KHATA')",
            params![date_prefix],
            |row| row.get(0),
        )?;

        let paid_jam_pages: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(jam_deducted_pages), 0)
             FROM sessions
             WHERE started_at >= ?1 AND started_at < (?1 || char(127))
               AND status IN ('PAID_UPI', 'PAID_CASH', 'KHATA')",
            params![date_prefix],
            |row| row.get(0),
        )?;

        let void_or_unbilled_pages: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(pj.effective_pages), 0)
             FROM print_jobs pj
             LEFT JOIN sessions s ON pj.session_id = s.id
             WHERE pj.captured_at >= ?1 AND pj.captured_at < (?1 || char(127))
               AND (s.id IS NULL OR s.status IN ('VOID', 'ACTIVE'))",
            params![date_prefix],
            |row| row.get(0),
        )?;

        let billed_pages = paid_job_pages.saturating_sub(paid_jam_pages).max(0);
        let accounted_pages = billed_pages
            .saturating_add(jam_deducted_pages)
            .saturating_add(void_or_unbilled_pages);
        let discrepancy_flag = void_or_unbilled_pages > 0 || accounted_pages != total_spooler_pages;

        Ok(DailyAuditReport {
            date_prefix: date_prefix.to_owned(),
            total_spooler_pages,
            billed_pages,
            jam_deducted_pages,
            void_or_unbilled_pages,
            upi_total_paise,
            cash_total_paise,
            khata_total_paise,
            discrepancy_flag,
        })
    }
}
