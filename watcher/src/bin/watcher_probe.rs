//! Live CLI probe verifying `printust_watcher` + `printust_core` against a local print queue.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use printust_core::{
    calculate_job_cost_for_modes, IngestOutcome, LedgerDb, SessionEngine,
};
use printust_watcher::{WatcherConfig, WatcherOutput, WatcherService};

const CTRL_C_EVENT: u32 = 0;

#[link(name = "kernel32")]
extern "system" {
    fn SetConsoleCtrlHandler(
        handler_routine: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

static CTRL_C_FLAG: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> i32 {
    if ctrl_type == CTRL_C_EVENT {
        CTRL_C_FLAG.store(true, Ordering::Release);
        return 1;
    }
    0
}

struct CliArgs {
    printer: String,
    db_path: Option<PathBuf>,
}

fn parse_cli_args() -> Result<CliArgs, String> {
    let mut printer = "ShopTest_PCL6".to_owned();
    let mut db_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--printer" => {
                printer = args
                    .next()
                    .ok_or_else(|| "missing value after --printer".to_owned())?;
            }
            "--db" => {
                let raw = args
                    .next()
                    .ok_or_else(|| "missing value after --db".to_owned())?;
                db_path = Some(PathBuf::from(raw));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: watcher_probe [--printer ShopTest_PCL6] [--db <path_to_sqlite.db>]"
                );
                std::process::exit(0);
            }
            other => {
                return Err(format!("unknown argument '{other}'"));
            }
        }
    }

    Ok(CliArgs { printer, db_path })
}

fn open_probe_db(db_path: Option<&PathBuf>) -> Result<(LedgerDb, String), String> {
    match db_path {
        Some(path) => {
            let db = LedgerDb::open_file(path).map_err(|e| e.to_string())?;
            Ok((db, path.display().to_string()))
        }
        None => {
            let db = LedgerDb::open_in_memory().map_err(|e| e.to_string())?;
            Ok((db, ":memory:".to_owned()))
        }
    }
}

fn run_probe() -> Result<(), String> {
    let cli = parse_cli_args()?;
    // SAFETY: Static C callback sets an `AtomicBool` flag cleanly on Ctrl+C.
    let _ = unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), 1) };

    let (db, db_label) = open_probe_db(cli.db_path.as_ref())?;
    let config = WatcherConfig::for_printer(&cli.printer);
    let initial_bookmark = db
        .load_bookmark(&config.channel_name)
        .map_err(|e| e.to_string())?;

    println!(
        "Printust Watcher Probe starting: printer='{}', db='{}', saved_bookmark={}",
        config.printer_name,
        db_label,
        initial_bookmark.is_some()
    );

    let (handle, rx) =
        WatcherService::start(config.clone(), initial_bookmark).map_err(|e| e.to_string())?;

    let mut engine = SessionEngine::new(db);
    println!(
        "Listening for Event 805/307 & DEVMODEW on '{}'. Press Ctrl+C to stop.",
        cli.printer
    );
    let _ = std::io::stdout().flush();

    while !CTRL_C_FLAG.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(WatcherOutput::JobReady(correlated)) => {
                let job = &correlated.job;
                let rules = engine.db().load_pricing_rules().map_err(|e| e.to_string())?;
                let (pages_per_copy, copies) = job.resolved_pricing_inputs();
                let paise = calculate_job_cost_for_modes(
                    pages_per_copy,
                    copies,
                    job.color_mode,
                    job.duplex_mode,
                    &rules,
                );

                let bookmark_status = match correlated.bookmark_xml.as_deref() {
                    Some(bm) => {
                        match engine.ingest_print_job_with_bookmark(
                            job.clone(),
                            &config.channel_name,
                            bm,
                        ) {
                            Ok(IngestOutcome::Applied(summary)) => format!(
                                "saved_atomic (session={}, session_total={}p)",
                                summary.session_id, summary.total_amount_paise
                            ),
                            Ok(IngestOutcome::DuplicateIgnored) => {
                                "duplicate_ignored (bookmark_advanced)".to_owned()
                            }
                            Err(e) => return Err(e.to_string()),
                        }
                    }
                    None => match engine.ingest_print_job_idempotent(job.clone()) {
                        Ok(IngestOutcome::Applied(summary)) => format!(
                            "no_bookmark (session={}, session_total={}p)",
                            summary.session_id, summary.total_amount_paise
                        ),
                        Ok(IngestOutcome::DuplicateIgnored) => "duplicate_ignored".to_owned(),
                        Err(e) => return Err(e.to_string()),
                    },
                };

                println!(
                    "[CAPTURED] job_id={} rec_id={:?} printer='{}' doc='{}' pages={} copies={} effective={} duplex={} color={} cost={}p anomaly={:?} late_805={} bookmark={}",
                    job.windows_job_id,
                    job.event_record_id,
                    job.printer_name,
                    job.doc_name,
                    job.raw_pages,
                    job.raw_copies,
                    job.effective_pages,
                    job.duplex_mode,
                    job.color_mode,
                    paise,
                    job.resolved_anomaly_flag(),
                    correlated.used_late_805,
                    bookmark_status
                );
                let _ = std::io::stdout().flush();
            }
            Ok(WatcherOutput::WarningOrError(err)) => {
                eprintln!("[WATCHER WARNING] {err}");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    println!("Stopping watcher cleanly...");
    handle.stop();
    println!("Watcher stopped cleanly.");
    Ok(())
}

fn main() {
    if let Err(err) = run_probe() {
        eprintln!("watcher_probe error: {err}");
        std::process::exit(1);
    }
}
