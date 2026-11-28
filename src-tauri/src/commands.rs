//! Typed Tauri v2 IPC commands and managed application state for Printust.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use printust_core::{
    DailyAuditReport, JamInkKind, PricingRules, SessionEngine, SessionStatus, SessionSummary,
};
use printust_watcher::WatcherHandle;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Tauri event emitted whenever the active billing session changes.
pub const EVENT_SESSION_UPDATED: &str = "printust://session-updated";

/// Tauri event emitted whenever watcher health or error status changes.
pub const EVENT_WATCHER_STATUS: &str = "printust://watcher-status";

/// Live health snapshot of the background Win32 spooler & Event Log watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherStatus {
    pub running: bool,
    pub printer_name: String,
    pub last_error: Option<String>,
    pub last_captured_at: Option<String>,
    pub restart_count: u32,
}

/// Shared Tauri-managed application state wiring SQLite `SessionEngine` and background watcher.
pub struct AppState {
    pub engine: Arc<Mutex<SessionEngine>>,
    pub watcher_handle: Arc<Mutex<Option<WatcherHandle>>>,
    pub watcher_status: Arc<Mutex<WatcherStatus>>,
    pub channel_name: String,
}

/// Acquire a mutex guard without panicking; maps poison errors to typed `String` IPC errors.
pub fn lock_state<'a, T>(mutex: &'a Mutex<T>, name: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{name} state mutex poisoned"))
}

/// Format current UTC wall-clock time as `"YYYY-MM-DDTHH:MM:SSZ"` without extra crate deps.
pub fn utc_now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_unix_secs_iso_utc(secs)
}

/// Convert Unix epoch seconds into an ISO-8601 UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`).
pub fn format_unix_secs_iso_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let sod = epoch_secs % 86_400;
    let hour = sod / 3_600;
    let minute = (sod % 3_600) / 60;
    let second = sod % 60;

    // Howard Hinnant civil-from-days algorithm anchored at 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if month <= 2 { 1 } else { 0 };

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Return the currently open `ACTIVE` billing session snapshot, if one exists.
#[tauri::command]
pub fn get_active_session(state: State<'_, AppState>) -> Result<Option<SessionSummary>, String> {
    let engine = lock_state(&state.engine, "session_engine")?;
    engine.get_active_summary().map_err(|e| e.to_string())
}

/// Deduct one jammed/misprinted B&W or Color page from the currently active session.
#[tauri::command]
pub fn apply_jam_deduction(
    app: AppHandle,
    state: State<'_, AppState>,
    kind: JamInkKind,
) -> Result<Option<SessionSummary>, String> {
    let summary = {
        let mut engine = lock_state(&state.engine, "session_engine")?;
        engine
            .apply_jam_deduction(kind)
            .map_err(|e| e.to_string())?
    };
    if let Some(ref s) = summary {
        let _ = app.emit(EVENT_SESSION_UPDATED, s);
    }
    Ok(summary)
}

/// Add a non-print shop item (e.g., Spiral Binding, Lamination) to the active session.
#[tauri::command]
pub fn add_session_addon(
    app: AppHandle,
    state: State<'_, AppState>,
    label: String,
    qty: i64,
    unit_price_paise: i64,
) -> Result<Option<SessionSummary>, String> {
    let summary = {
        let mut engine = lock_state(&state.engine, "session_engine")?;
        engine
            .add_addon(&label, qty, unit_price_paise)
            .map_err(|e| e.to_string())?
    };
    if let Some(ref s) = summary {
        let _ = app.emit(EVENT_SESSION_UPDATED, s);
    }
    Ok(summary)
}

/// Finalize the active session under `PAID_UPI`, `PAID_CASH`, `KHATA`, or `VOID`.
#[tauri::command]
pub fn close_active_session(
    state: State<'_, AppState>,
    status: SessionStatus,
) -> Result<Option<SessionSummary>, String> {
    let mut engine = lock_state(&state.engine, "session_engine")?;
    engine
        .close_active_session(status, &utc_now_iso())
        .map_err(|e| e.to_string())
}

/// Reconcile captured OS spooler pages against billed totals, jams, and void leakage for a date.
#[tauri::command]
pub fn get_daily_audit(
    state: State<'_, AppState>,
    date_prefix: String,
) -> Result<DailyAuditReport, String> {
    let engine = lock_state(&state.engine, "session_engine")?;
    engine.get_daily_audit(&date_prefix).map_err(|e| e.to_string())
}

/// Load shop rate slabs stored strictly in integer paise.
#[tauri::command]
pub fn get_pricing_rules(state: State<'_, AppState>) -> Result<PricingRules, String> {
    let engine = lock_state(&state.engine, "session_engine")?;
    engine.db().load_pricing_rules().map_err(|e| e.to_string())
}

/// Validate and save updated integer-paise rate slabs atomically.
#[tauri::command]
pub fn update_pricing_rules(
    state: State<'_, AppState>,
    rules: PricingRules,
) -> Result<(), String> {
    let mut engine = lock_state(&state.engine, "session_engine")?;
    engine
        .db_mut()
        .save_pricing_rules(&rules)
        .map_err(|e| e.to_string())
}

/// Return current background spooler watcher health and capture metadata.
#[tauri::command]
pub fn get_watcher_status(state: State<'_, AppState>) -> Result<WatcherStatus, String> {
    let status = lock_state(&state.watcher_status, "watcher_status")?;
    Ok(status.clone())
}

#[cfg(test)]
mod tests {
    use super::format_unix_secs_iso_utc;

    #[test]
    fn formats_unix_epoch_and_known_utc_timestamps_without_dependencies() {
        assert_eq!(format_unix_secs_iso_utc(0), "1970-01-01T00:00:00Z");
        // 2026-11-27T15:30:45Z
        assert_eq!(
            format_unix_secs_iso_utc(1_795_793_445),
            "2026-11-27T15:30:45Z"
        );
    }
}
