use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use printust_core::{
    ColorMode, DuplexMode, IncomingPrintJob, IngestOutcome, JamInkKind, LedgerDb, LedgerError,
    PricingRules, SessionEngine, SessionStatus, SessionSummary,
};

fn make_job(
    job_id: u32,
    doc: &str,
    raw_pages: u32,
    raw_copies: u32,
    is_duplex: bool,
    is_color: bool,
    captured_at: &str,
) -> IncomingPrintJob {
    IncomingPrintJob {
        windows_job_id: job_id,
        event_record_id: None,
        printer_name: "ShopTest_PCL6".to_owned(),
        doc_name: doc.to_owned(),
        raw_pages,
        raw_copies,
        effective_pages: raw_pages.saturating_mul(raw_copies),
        is_duplex,
        is_color,
        duplex_mode: DuplexMode::from_bool(is_duplex),
        color_mode: ColorMode::from_bool(is_color),
        driver_anomaly_flag: None,
        captured_at: captured_at.to_owned(),
    }
}

#[test]
fn auto_opens_session_and_appends_multiple_jobs() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);
    assert!(engine.get_active_summary().unwrap().is_none());

    // Job 1: 3-page B&W x 2 copies duplex -> (300 + 200) * 2 = 1000 paise
    let s1 = engine
        .ingest_print_job(make_job(
            101,
            "notes.pdf",
            3,
            2,
            true,
            false,
            "2026-11-27T10:00:00Z",
        ))
        .unwrap();
    assert_eq!(s1.status, SessionStatus::Active);
    assert_eq!(s1.job_count, 1);
    assert_eq!(s1.bw_pages, 6);
    assert_eq!(s1.color_pages, 0);
    assert_eq!(s1.total_amount_paise, 1000);
    assert!(!s1.has_anomaly);
    assert_eq!(s1.last_doc_name.as_deref(), Some("notes.pdf"));

    // Job 2: 2-page Color x 1 copy simplex -> 2 * 1000 = 2000 paise -> cumulative 3000 paise
    let mut job2 = make_job(
        102,
        "cover.pdf",
        2,
        1,
        false,
        true,
        "2026-11-27T10:00:20Z",
    );
    job2.driver_anomaly_flag = Some("DUPLEX_UNKNOWN".to_owned());
    let s2 = engine.ingest_print_job(job2).unwrap();
    assert_eq!(s2.session_id, s1.session_id);
    assert_eq!(s2.job_count, 2);
    assert_eq!(s2.bw_pages, 6);
    assert_eq!(s2.color_pages, 2);
    assert_eq!(s2.total_amount_paise, 3000);
    assert!(s2.has_anomaly);
    assert_eq!(s2.last_doc_name.as_deref(), Some("cover.pdf"));
}

#[test]
fn addons_update_session_total_and_addons_sum() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);

    // No active session yet -> returns Ok(None)
    assert!(engine.add_addon("Spiral Binding", 1, 3000).unwrap().is_none());

    // 4-page B&W simplex = 800 paise
    engine
        .ingest_print_job(make_job(
            201,
            "thesis.pdf",
            4,
            1,
            false,
            false,
            "2026-11-27T10:15:00Z",
        ))
        .unwrap();

    // + Spiral Binding Rs. 30 (3000) and 2x Lamination Rs. 20 (4000) -> total 7800
    engine.add_addon("Spiral Binding", 1, 3000).unwrap().unwrap();
    let summary = engine.add_addon("Lamination", 2, 2000).unwrap().unwrap();

    assert_eq!(summary.addons_total_paise, 7000);
    assert_eq!(summary.total_amount_paise, 7800);
}

#[test]
fn jam_deduction_increments_count_and_clamps_total_at_zero() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);

    assert!(engine
        .apply_jam_deduction(JamInkKind::Bw)
        .unwrap()
        .is_none());

    // 1-page B&W simplex = 200 paise
    engine
        .ingest_print_job(make_job(
            301,
            "receipt.pdf",
            1,
            1,
            false,
            false,
            "2026-11-27T10:30:00Z",
        ))
        .unwrap();

    // Cannot deduct Color jam when session has 0 Color sides
    let color_err = engine.apply_jam_deduction(JamInkKind::Color).unwrap_err();
    assert!(matches!(color_err, LedgerError::InvalidInput(_)));

    let after_one = engine.apply_jam_deduction(JamInkKind::Bw).unwrap().unwrap();
    assert_eq!(after_one.jam_deducted_pages, 1);
    assert_eq!(after_one.bw_jam_deducted_pages, 1);
    assert_eq!(after_one.color_jam_deducted_pages, 0);
    assert_eq!(after_one.total_amount_paise, 0);

    // Second jam keeps total clamped at 0 (never negative) while recording jam count
    let after_two = engine.apply_jam_deduction(JamInkKind::Bw).unwrap().unwrap();
    assert_eq!(after_two.jam_deducted_pages, 2);
    assert_eq!(after_two.bw_jam_deducted_pages, 2);
    assert_eq!(after_two.total_amount_paise, 0);
}

#[test]
fn closing_session_finalizes_it_and_next_job_opens_new_session() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);

    let first = engine
        .ingest_print_job(make_job(
            401,
            "cust1.pdf",
            2,
            1,
            false,
            false,
            "2026-11-27T11:00:00Z",
        ))
        .unwrap();

    // Closing with Active must fail
    let bad_close = engine
        .close_active_session(SessionStatus::Active, "2026-11-27T11:01:00Z")
        .unwrap_err();
    assert!(matches!(bad_close, LedgerError::InvalidInput(_)));

    let closed = engine
        .close_active_session(SessionStatus::PaidUpi, "2026-11-27T11:02:00Z")
        .unwrap()
        .unwrap();
    assert_eq!(closed.session_id, first.session_id);
    assert_eq!(closed.status, SessionStatus::PaidUpi);
    assert_eq!(closed.closed_at.as_deref(), Some("2026-11-27T11:02:00Z"));
    assert!(engine.get_active_summary().unwrap().is_none());

    let second = engine
        .ingest_print_job(make_job(
            402,
            "cust2.pdf",
            1,
            1,
            false,
            false,
            "2026-11-27T11:05:00Z",
        ))
        .unwrap();
    assert_ne!(second.session_id, first.session_id);
    assert_eq!(second.status, SessionStatus::Active);
    assert_eq!(second.total_amount_paise, 200);
}

#[test]
fn daily_audit_report_separates_payment_modes_and_flags_void_leakage() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);

    // Session 1 (PAID_UPI): 4 pages B&W simplex = 800 paise
    engine
        .ingest_print_job(make_job(
            501,
            "u.pdf",
            4,
            1,
            false,
            false,
            "2026-11-27T12:00:00Z",
        ))
        .unwrap();
    engine
        .close_active_session(SessionStatus::PaidUpi, "2026-11-27T12:01:00Z")
        .unwrap();

    // Session 2 (PAID_CASH): 3 pages B&W simplex (600) - 1 jam (200) = 400 paise
    engine
        .ingest_print_job(make_job(
            502,
            "c.pdf",
            3,
            1,
            false,
            false,
            "2026-11-27T12:10:00Z",
        ))
        .unwrap();
    engine.apply_jam_deduction(JamInkKind::Bw).unwrap();
    engine
        .close_active_session(SessionStatus::PaidCash, "2026-11-27T12:11:00Z")
        .unwrap();

    // Session 3 (KHATA): 2 pages B&W simplex = 400 paise
    engine
        .ingest_print_job(make_job(
            503,
            "k.pdf",
            2,
            1,
            false,
            false,
            "2026-11-27T12:20:00Z",
        ))
        .unwrap();
    engine
        .close_active_session(SessionStatus::Khata, "2026-11-27T12:21:00Z")
        .unwrap();

    // Clean report before any VOID session -> discrepancy_flag == false
    let clean_report = engine.get_daily_audit("2026-11-27").unwrap();
    assert_eq!(clean_report.total_spooler_pages, 9);
    assert_eq!(clean_report.billed_pages, 8);
    assert_eq!(clean_report.jam_deducted_pages, 1);
    assert_eq!(clean_report.void_or_unbilled_pages, 0);
    assert_eq!(clean_report.upi_total_paise, 800);
    assert_eq!(clean_report.cash_total_paise, 400);
    assert_eq!(clean_report.khata_total_paise, 400);
    assert!(!clean_report.discrepancy_flag);

    // Session 4 (VOID): 5 unbilled leaked pages
    engine
        .ingest_print_job(make_job(
            504,
            "void.pdf",
            5,
            1,
            false,
            false,
            "2026-11-27T12:30:00Z",
        ))
        .unwrap();
    engine
        .close_active_session(SessionStatus::Void, "2026-11-27T12:31:00Z")
        .unwrap();

    let leak_report = engine.get_daily_audit("2026-11-27").unwrap();
    assert_eq!(leak_report.total_spooler_pages, 14);
    assert_eq!(leak_report.billed_pages, 8);
    assert_eq!(leak_report.jam_deducted_pages, 1);
    assert_eq!(leak_report.void_or_unbilled_pages, 5);
    assert!(leak_report.discrepancy_flag);
}

#[test]
fn pricing_rules_roundtrip_negative_guard_wal_pragmas_and_unrolled_fallback() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("printust_test_{unique}"));
    let db_path = temp_dir.join("ledger.db");

    {
        let mut db = LedgerDb::open_file(&db_path).unwrap();

        // Verify WAL, foreign_keys, and busy_timeout PRAGMAs applied on open
        let journal_mode: String = db
            .connection()
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .unwrap();
        let foreign_keys: i64 = db
            .connection()
            .query_row("PRAGMA foreign_keys;", [], |r| r.get(0))
            .unwrap();
        let busy_timeout: i64 = db
            .connection()
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_lowercase(), "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(busy_timeout, 5000);

        // SQLite CHECK(rate_paise >= 0) rejects direct negative insert too
        let sql_neg = db.connection().execute(
            "INSERT INTO pricing_rules (rule_key, rate_paise) VALUES ('BAD', -1)",
            [],
        );
        assert!(sql_neg.is_err());

        // save_pricing_rules rejects negative rates before opening a write tx
        let bad_rules = PricingRules {
            bw_single_paise: -50,
            ..PricingRules::default()
        };
        assert!(matches!(
            db.save_pricing_rules(&bad_rules).unwrap_err(),
            LedgerError::InvalidInput(_)
        ));

        let custom = PricingRules {
            bw_single_paise: 150,
            bw_duplex_sheet_paise: 250,
            color_single_paise: 800,
            color_duplex_sheet_paise: 1500,
            bw_bulk_threshold_sides: 40,
            bw_bulk_side_paise: 90,
        };
        db.save_pricing_rules(&custom).unwrap();
        assert_eq!(db.load_pricing_rules().unwrap(), custom);
    }

    // Reopen file DB and confirm rules + unrolled Event 307 fallback without double-multiplying
    {
        let db = LedgerDb::open_file(&db_path).unwrap();
        let reloaded = db.load_pricing_rules().unwrap();
        assert_eq!(reloaded.bw_single_paise, 150);

        let mut engine = SessionEngine::new(db);
        // Simulate GetJobW miss on 3p x 2c job: raw_pages=6 (unrolled 307), raw_copies=2 (805), effective=6
        let mut fallback_job = make_job(
            601,
            "Print Document",
            6,
            2,
            false,
            false,
            "2026-11-27T13:00:00Z",
        );
        fallback_job.effective_pages = 6;
        fallback_job.driver_anomaly_flag = Some("DUPLEX_UNKNOWN".to_owned());
        let summary = engine.ingest_print_job(fallback_job).unwrap();
        // Must bill 6 * 150 = 900 paise (NOT 12 * 150 = 1800)
        assert_eq!(summary.total_amount_paise, 900);
        assert!(summary.has_anomaly);

        let json = serde_json::to_string(&summary).unwrap();
        let decoded: SessionSummary = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, summary);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn single_active_session_partial_unique_index_enforced() {
    let db = LedgerDb::open_in_memory().unwrap();
    let conn = db.connection();

    // First ACTIVE session succeeds
    conn.execute(
        "INSERT INTO sessions (started_at, status) VALUES ('2026-11-27T10:00:00Z', 'ACTIVE')",
        [],
    )
    .unwrap();

    // Second concurrent ACTIVE session is blocked at the SQLite engine level
    let duplicate_active = conn.execute(
        "INSERT INTO sessions (started_at, status) VALUES ('2026-11-27T10:05:00Z', 'ACTIVE')",
        [],
    );
    assert!(duplicate_active.is_err());

    // Multiple historical/closed sessions remain allowed under the partial unique index
    for (ts, status) in [
        ("2026-11-27T09:00:00Z", "PAID_UPI"),
        ("2026-11-27T09:15:00Z", "PAID_UPI"),
        ("2026-11-27T09:30:00Z", "VOID"),
        ("2026-11-27T09:45:00Z", "VOID"),
    ] {
        conn.execute(
            "INSERT INTO sessions (started_at, status) VALUES (?1, ?2)",
            [ts, status],
        )
        .unwrap();
    }
}

#[test]
fn daily_audit_range_queries_use_btree_indexes() {
    let db = LedgerDb::open_in_memory().unwrap();
    let conn = db.connection();

    let explain = |sql: &str| -> Vec<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map(["2026-11-27"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };

    let print_jobs_plan = explain(
        "EXPLAIN QUERY PLAN
         SELECT COALESCE(SUM(effective_pages), 0)
         FROM print_jobs
         WHERE captured_at >= ?1 AND captured_at < (?1 || char(127))",
    );
    assert!(
        print_jobs_plan
            .iter()
            .any(|line| line.contains("USING INDEX idx_print_jobs_captured_at")),
        "expected B-Tree index search on print_jobs.captured_at, got: {print_jobs_plan:?}"
    );
    assert!(
        !print_jobs_plan.iter().any(|line| line.starts_with("SCAN ")),
        "unexpected full table scan on print_jobs: {print_jobs_plan:?}"
    );

    let sessions_plan = explain(
        "EXPLAIN QUERY PLAN
         SELECT COALESCE(SUM(jam_deducted_pages), 0)
         FROM sessions
         WHERE started_at >= ?1 AND started_at < (?1 || char(127))",
    );
    assert!(
        sessions_plan
            .iter()
            .any(|line| line.contains("USING INDEX idx_sessions_started_at")),
        "expected B-Tree index search on sessions.started_at, got: {sessions_plan:?}"
    );
    assert!(
        !sessions_plan.iter().any(|line| line.starts_with("SCAN ")),
        "unexpected full table scan on sessions: {sessions_plan:?}"
    );
}

#[test]
fn idempotent_dedup_atomic_bookmark_and_unknown_duplex_fallback() {
    let channel = "Microsoft-Windows-PrintService/Operational";
    let db = LedgerDb::open_in_memory().unwrap();
    assert!(db.load_bookmark(channel).unwrap().is_none());

    let mut engine = SessionEngine::new(db);

    // 1. DuplexMode::Unknown job with no explicit flag -> falls back to simplex + DEVMODE_RACE_MISSED
    let mut job1 = make_job(
        701,
        "notes.pdf",
        3,
        1,
        false,
        false,
        "2026-11-27T14:00:00Z",
    );
    job1.event_record_id = Some(9001);
    job1.duplex_mode = DuplexMode::Unknown;
    job1.color_mode = ColorMode::Monochrome;

    let bookmark_v1 = "<BookmarkList><Bookmark Channel='Microsoft-Windows-PrintService/Operational' RecordId='9001'/></BookmarkList>";
    let outcome1 = engine
        .ingest_print_job_with_bookmark(job1.clone(), channel, bookmark_v1)
        .unwrap();
    match outcome1 {
        IngestOutcome::Applied(summary) => {
            assert_eq!(summary.total_amount_paise, 600); // 3 * 200 simplex fallback
            assert!(summary.has_anomaly);
        }
        IngestOutcome::DuplicateIgnored => panic!("expected first job to be applied"),
    }
    assert_eq!(
        engine.db().load_bookmark(channel).unwrap().as_deref(),
        Some(bookmark_v1)
    );
    let stored_flag: Option<String> = engine
        .db()
        .connection()
        .query_row(
            "SELECT driver_anomaly_flag FROM print_jobs WHERE event_record_id = 9001",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored_flag.as_deref(), Some("DEVMODE_RACE_MISSED"));

    // 2. Close session and replay the same (printer_name, event_record_id) -> ignored cleanly,
    //    must not reopen a phantom ACTIVE session or double-bill
    engine
        .close_active_session(SessionStatus::PaidUpi, "2026-11-27T14:01:00Z")
        .unwrap();
    let bookmark_v2 = "<BookmarkList><Bookmark Channel='Microsoft-Windows-PrintService/Operational' RecordId='9002'/></BookmarkList>";
    let dup_outcome = engine
        .ingest_print_job_with_bookmark(job1, channel, bookmark_v2)
        .unwrap();
    assert_eq!(dup_outcome, IngestOutcome::DuplicateIgnored);
    assert!(engine.get_active_summary().unwrap().is_none());
    // Bookmark advances cleanly even when skipping a duplicate event
    assert_eq!(
        engine.db().load_bookmark(channel).unwrap().as_deref(),
        Some(bookmark_v2)
    );

    // 3. Invalid empty bookmark fails before writing job (atomic rollback safety)
    let mut job2 = make_job(
        702,
        "next.pdf",
        2,
        1,
        false,
        false,
        "2026-11-27T14:02:00Z",
    );
    job2.event_record_id = Some(9003);
    assert!(engine
        .ingest_print_job_with_bookmark(job2, channel, "   ")
        .is_err());
    assert!(engine.get_active_summary().unwrap().is_none());
}


#[test]
fn mixed_session_bw_and_color_jam_deductions_and_append_only_jobs() {
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);

    // 1. Mixed session: 13p B&W Duplex (6*300 + 200 = 2000p) + 12p Color Duplex (6*1800 = 10800p)
    let job_bw = make_job(
        801,
        "notes_bw.pdf",
        13,
        1,
        true,
        false,
        "2026-11-28T10:00:00Z",
    );
    let job_color = make_job(
        802,
        "diagrams_color.pdf",
        12,
        1,
        true,
        true,
        "2026-11-28T10:00:20Z",
    );
    engine.ingest_print_job(job_bw).unwrap();
    let before_jam = engine.ingest_print_job(job_color).unwrap();
    assert_eq!(before_jam.total_amount_paise, 12_800);
    assert_eq!(before_jam.bw_jam_rate_paise, 200);
    assert_eq!(before_jam.color_jam_rate_paise, 1000);
    assert_eq!(before_jam.unverified_jobs_count, 0);
    assert_eq!(before_jam.jobs.len(), 2);
    // Ordered newest-first so active HUD puts most recent print at the top
    assert_eq!(before_jam.jobs[0].doc_name, "diagrams_color.pdf");
    assert_eq!(before_jam.jobs[0].cost_paise, 10_800);
    assert_eq!(before_jam.jobs[0].duplex_mode, DuplexMode::DuplexLongEdge);
    assert_eq!(before_jam.jobs[0].color_mode, ColorMode::Color);
    assert_eq!(before_jam.jobs[1].doc_name, "notes_bw.pdf");
    assert_eq!(before_jam.jobs[1].cost_paise, 2000);

    // Deduct 1 Color jam -> exact 1000p deducted (12800 -> 11800)
    let after_color_jam = engine.apply_jam_deduction(JamInkKind::Color).unwrap().unwrap();
    assert_eq!(after_color_jam.total_amount_paise, 11_800);
    assert_eq!(after_color_jam.bw_jam_deducted_pages, 0);
    assert_eq!(after_color_jam.color_jam_deducted_pages, 1);
    assert_eq!(after_color_jam.jam_deducted_pages, 1);

    // Deduct 1 B&W jam -> exact 200p deducted (11800 -> 11600)
    let after_bw_jam = engine.apply_jam_deduction(JamInkKind::Bw).unwrap().unwrap();
    assert_eq!(after_bw_jam.total_amount_paise, 11_600);
    assert_eq!(after_bw_jam.bw_jam_deducted_pages, 1);
    assert_eq!(after_bw_jam.color_jam_deducted_pages, 1);
    assert_eq!(after_bw_jam.jam_deducted_pages, 2);

    // Verify `print_jobs` stayed strictly append-only (job counts, pages, and costs unchanged)
    assert_eq!(after_bw_jam.jobs.len(), 2);
    assert_eq!(after_bw_jam.jobs[0].cost_paise, 10_800);
    assert_eq!(after_bw_jam.jobs[1].cost_paise, 2000);

    // 2. Color-only session rejects B&W jam deduction and clamps Color jam total at 0
    engine
        .close_active_session(SessionStatus::PaidUpi, "2026-11-28T10:05:00Z")
        .unwrap();
    engine
        .ingest_print_job(make_job(
            803,
            "photo_only.pdf",
            1,
            1,
            false,
            true,
            "2026-11-28T10:10:00Z",
        ))
        .unwrap();

    let bw_err = engine.apply_jam_deduction(JamInkKind::Bw).unwrap_err();
    assert!(matches!(bw_err, LedgerError::InvalidInput(_)));

    // 1000p - 1000p = 0p, then second color jam clamps safely at 0p
    let c1 = engine.apply_jam_deduction(JamInkKind::Color).unwrap().unwrap();
    assert_eq!(c1.total_amount_paise, 0);
    assert_eq!(c1.color_jam_deducted_pages, 1);
    let c2 = engine.apply_jam_deduction(JamInkKind::Color).unwrap().unwrap();
    assert_eq!(c2.total_amount_paise, 0);
    assert_eq!(c2.color_jam_deducted_pages, 2);
}

#[test]
fn session_summary_surfaces_unverified_unknown_modes_and_legacy_db_migration() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("printust_legacy_mig_{unique}"));
    fs::create_dir_all(&temp_dir).unwrap();
    let db_path = temp_dir.join("legacy.db");

    // Simulate a pre-patch SQLite database lacking `bw_jam_deducted_pages`,
    // `color_jam_deducted_pages`, and `print_jobs.cost_paise`.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                started_at TEXT NOT NULL,
                closed_at TEXT,
                status TEXT NOT NULL CHECK(status IN ('ACTIVE','PAID_UPI','PAID_CASH','KHATA','VOID')),
                total_amount_paise INTEGER NOT NULL DEFAULT 0,
                jam_deducted_pages INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE print_jobs (
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
             INSERT INTO sessions (id, started_at, status, total_amount_paise, jam_deducted_pages)
             VALUES (1, '2026-11-28T09:00:00Z', 'ACTIVE', 2000, 0);
             INSERT INTO print_jobs (
                session_id, windows_job_id, printer_name, doc_name,
                raw_pages, raw_copies, effective_pages, is_duplex, is_color,
                driver_anomaly_flag, captured_at
             ) VALUES (
                1, 91, 'ShopTest_PCL6', 'legacy_scan.pdf',
                13, 1, 13, 1, 0, NULL, '2026-11-28T09:00:01Z'
             );",
        )
        .unwrap();
    }

    {
        let db = LedgerDb::open_file(&db_path).unwrap();
        let mut engine = SessionEngine::new(db);

        // Legacy row gets fallback cost recomputed cleanly (6*300 + 200 = 2000p) and
        // Unknown duplex/color flags surface in unverified_jobs_count.
        let legacy_summary = engine.get_active_summary().unwrap().unwrap();
        assert_eq!(legacy_summary.jobs.len(), 1);
        assert_eq!(legacy_summary.jobs[0].cost_paise, 2000);
        assert_eq!(legacy_summary.unverified_jobs_count, 1);
        assert!(legacy_summary.has_anomaly);
        assert!(legacy_summary
            .anomaly_reasons
            .contains(&"DUPLEX_UNKNOWN".to_owned()));
        assert!(legacy_summary
            .anomaly_reasons
            .contains(&"COLOR_UNKNOWN".to_owned()));

        // Verified job does not inflate unverified_jobs_count
        engine
            .ingest_print_job(make_job(
                92,
                "verified_color.pdf",
                2,
                1,
                false,
                true,
                "2026-11-28T09:01:00Z",
            ))
            .unwrap();
        let after_second = engine.get_active_summary().unwrap().unwrap();
        assert_eq!(after_second.job_count, 2);
        assert_eq!(after_second.unverified_jobs_count, 1);
        assert_eq!(after_second.jobs[0].cost_paise, 2000);
        assert!(!after_second.jobs[0].is_unverified);
        assert!(after_second.jobs[1].is_unverified);
    }

    let _ = fs::remove_dir_all(&temp_dir);
}
