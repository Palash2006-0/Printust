//! Tauri v2 desktop host wiring `printust_watcher` -> `printust_core` -> Mini-HUD events.

pub mod commands;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use commands::{
    lock_state, AppState, WatcherStatus, EVENT_SESSION_UPDATED, EVENT_WATCHER_STATUS,
};
use printust_core::{IngestOutcome, LedgerDb, SessionEngine};
use printust_watcher::xml_parser::WatcherError;
use printust_watcher::{
    WatcherConfig, WatcherHandle, WatcherOutput, WatcherService, DEFAULT_EVENT_CHANNEL,
};
use tauri::{AppHandle, Emitter, Manager, RunEvent};

const DEFAULT_PRINTER_NAME: &str = "ShopTest_PCL6";
const MAX_LIVE_RESTARTS_PER_WINDOW: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(30);

fn stop_active_watcher(slot: &Arc<Mutex<Option<WatcherHandle>>>) {
    // Take the handle and release the MutexGuard *before* blocking inside `.stop()`.
    let active = slot.lock().ok().and_then(|mut guard| guard.take());
    if let Some(handle) = active {
        handle.stop();
    }
}

fn configured_printer_name() -> String {
    std::env::var("PRINTUST_PRINTER")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PRINTER_NAME.to_owned())
}

fn update_status_and_emit(
    app: &AppHandle,
    status_lock: &Arc<Mutex<WatcherStatus>>,
    updater: impl FnOnce(&mut WatcherStatus),
) {
    if let Ok(mut guard) = status_lock.lock() {
        updater(&mut guard);
        let snapshot = guard.clone();
        drop(guard);
        let _ = app.emit(EVENT_WATCHER_STATUS, &snapshot);
    }
}

fn spawn_watcher_supervisor(
    app: AppHandle,
    engine: Arc<Mutex<SessionEngine>>,
    watcher_handle_slot: Arc<Mutex<Option<WatcherHandle>>>,
    watcher_status: Arc<Mutex<WatcherStatus>>,
    shutdown_flag: Arc<AtomicBool>,
    config: WatcherConfig,
    initial_bookmark: Option<String>,
) -> Result<JoinHandle<()>, String> {
    let mut active_rx = match WatcherService::start(config.clone(), initial_bookmark) {
        Ok((handle, rx)) => {
            *lock_state(&watcher_handle_slot, "watcher_handle")? = Some(handle);
            update_status_and_emit(&app, &watcher_status, |s| {
                s.running = true;
                s.last_error = None;
            });
            rx
        }
        Err(err) => {
            update_status_and_emit(&app, &watcher_status, |s| {
                s.running = false;
                s.last_error = Some(err.to_string());
            });
            return Err(err.to_string());
        }
    };

    thread::Builder::new()
        .name("printust-tauri-supervisor".to_owned())
        .spawn(move || {
            let poll = Duration::from_millis(200);
            let mut recent_restarts: VecDeque<Instant> = VecDeque::new();
            while !shutdown_flag.load(Ordering::Acquire) {
                match active_rx.recv_timeout(poll) {
                    Ok(WatcherOutput::JobReady(correlated)) => {
                        let captured_at = correlated.job.captured_at.clone();
                        let ingest_res = match engine.lock() {
                            Ok(mut eng) => match correlated.bookmark_xml.as_deref() {
                                Some(bm) => eng
                                    .ingest_print_job_with_bookmark(
                                        correlated.job,
                                        &config.channel_name,
                                        bm,
                                    )
                                    .map_err(|e| e.to_string()),
                                None => eng
                                    .ingest_print_job_idempotent(correlated.job)
                                    .map_err(|e| e.to_string()),
                            },
                            Err(_) => Err("session_engine state mutex poisoned".to_owned()),
                        };

                        match ingest_res {
                            Ok(IngestOutcome::Applied(summary)) => {
                                update_status_and_emit(&app, &watcher_status, |s| {
                                    s.running = true;
                                    s.last_captured_at = Some(captured_at);
                                    s.last_error = None;
                                });
                                let _ = app.emit(EVENT_SESSION_UPDATED, &summary);
                            }
                            Ok(IngestOutcome::DuplicateIgnored) => {
                                update_status_and_emit(&app, &watcher_status, |s| {
                                    s.running = true;
                                    s.last_captured_at = Some(captured_at);
                                });
                            }
                            Err(msg) => {
                                update_status_and_emit(&app, &watcher_status, |s| {
                                    s.last_error = Some(msg);
                                });
                            }
                        }
                    }
                    Ok(WatcherOutput::WarningOrError(err)) => {
                        let err_text = err.to_string();
                        let needs_live_restart = matches!(
                            err,
                            WatcherError::Win32 {
                                api: "EvtSubscribe(callback)",
                                ..
                            }
                        );

                        update_status_and_emit(&app, &watcher_status, |s| {
                            s.last_error = Some(err_text);
                        });

                        // Carry-forward safeguard: if Windows Event Log was cleared live (`wevtutil cl`),
                        // restart `WatcherService` cleanly in future-events mode (rate-limited against spin loops).
                        if needs_live_restart && !shutdown_flag.load(Ordering::Acquire) {
                            let now = Instant::now();
                            while recent_restarts
                                .front()
                                .is_some_and(|t| now.saturating_duration_since(*t) > RESTART_WINDOW)
                            {
                                recent_restarts.pop_front();
                            }
                            if recent_restarts.len() >= MAX_LIVE_RESTARTS_PER_WINDOW {
                                stop_active_watcher(&watcher_handle_slot);
                                update_status_and_emit(&app, &watcher_status, |s| {
                                    s.running = false;
                                    s.last_error = Some(
                                        "Event Log reset loop limit reached; watcher paused to prevent spin"
                                            .to_owned(),
                                    );
                                });
                                break;
                            }
                            recent_restarts.push_back(now);

                            stop_active_watcher(&watcher_handle_slot);
                            thread::sleep(Duration::from_millis(400));
                            if shutdown_flag.load(Ordering::Acquire) {
                                break;
                            }
                            match WatcherService::start(config.clone(), None) {
                                Ok((new_handle, new_rx)) => {
                                    if let Ok(mut slot) = watcher_handle_slot.lock() {
                                        *slot = Some(new_handle);
                                    }
                                    active_rx = new_rx;
                                    update_status_and_emit(&app, &watcher_status, |s| {
                                        s.running = true;
                                        s.restart_count = s.restart_count.saturating_add(1);
                                        s.last_error = Some(
                                            "Event Log reset detected; restarted watcher on live future events".to_owned(),
                                        );
                                    });
                                }
                                Err(restart_err) => {
                                    update_status_and_emit(&app, &watcher_status, |s| {
                                        s.running = false;
                                        s.last_error = Some(restart_err.to_string());
                                    });
                                    break;
                                }
                            }
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        update_status_and_emit(&app, &watcher_status, |s| {
                            s.running = false;
                        });
                        break;
                    }
                }
            }
        })
        .map_err(|e| format!("failed to spawn watcher supervisor thread: {e}"))
}

/// Configure and launch the Printust Tauri v2 application.
pub fn run() {
    let printer_name = configured_printer_name();
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let watcher_handle_slot: Arc<Mutex<Option<WatcherHandle>>> = Arc::new(Mutex::new(None));
    let supervisor_thread_slot: Arc<Mutex<Option<JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let watcher_status = Arc::new(Mutex::new(WatcherStatus {
        running: false,
        printer_name: printer_name.clone(),
        last_error: None,
        last_captured_at: None,
        restart_count: 0,
    }));

    let shutdown_flag_on_exit = Arc::clone(&shutdown_flag);
    let watcher_handle_on_exit = Arc::clone(&watcher_handle_slot);
    let supervisor_on_exit = Arc::clone(&supervisor_thread_slot);

    let builder_res = tauri::Builder::default()
        .setup(move |app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("failed to resolve OS AppData dir: {e}"))?;
            let db_path = app_data_dir.join("printust.db");
            let db = LedgerDb::open_file(&db_path)?;
            let initial_bookmark = db.load_bookmark(DEFAULT_EVENT_CHANNEL)?;

            let engine = Arc::new(Mutex::new(SessionEngine::new(db)));
            let state = AppState {
                engine: Arc::clone(&engine),
                watcher_handle: Arc::clone(&watcher_handle_slot),
                watcher_status: Arc::clone(&watcher_status),
                channel_name: DEFAULT_EVENT_CHANNEL.to_owned(),
            };
            app.manage(state);

            let config = WatcherConfig::for_printer(&printer_name);
            let app_handle = app.handle().clone();
            match spawn_watcher_supervisor(
                app_handle.clone(),
                engine,
                Arc::clone(&watcher_handle_slot),
                Arc::clone(&watcher_status),
                Arc::clone(&shutdown_flag),
                config,
                initial_bookmark,
            ) {
                Ok(join_handle) => {
                    if let Ok(mut slot) = supervisor_thread_slot.lock() {
                        *slot = Some(join_handle);
                    }
                }
                Err(err_msg) => {
                    // Surface watcher startup failure cleanly in the UI without crashing the counter HUD.
                    update_status_and_emit(&app_handle, &watcher_status, |s| {
                        s.running = false;
                        s.last_error = Some(err_msg);
                    });
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_active_session,
            commands::apply_jam_deduction,
            commands::add_session_addon,
            commands::close_active_session,
            commands::get_daily_audit,
            commands::get_pricing_rules,
            commands::update_pricing_rules,
            commands::get_watcher_status,
        ])
        .build(tauri::generate_context!());

    match builder_res {
        Ok(app) => {
            app.run(move |_app_handle, event| {
                if let RunEvent::Exit = event {
                    shutdown_flag_on_exit.store(true, Ordering::Release);
                    stop_active_watcher(&watcher_handle_on_exit);
                    if let Ok(mut sup_slot) = supervisor_on_exit.lock() {
                        if let Some(join_handle) = sup_slot.take() {
                            let _ = join_handle.join();
                        }
                    }
                }
            });
        }
        Err(err) => {
            eprintln!("Printust startup failed: {err}");
        }
    }
}
