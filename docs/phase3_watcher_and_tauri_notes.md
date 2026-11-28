# Printust Phase 3 Architecture & Viva Reference (`watcher/` + `src-tauri/` + `ui/`)

Concise post-exam reference covering the Day 6 native Win32 spooler watcher (`printust_watcher`) and Day 7 Tauri v2 desktop Mini-HUD (`printust_app` + `ui/`).

---

## 1. Dual-Stream Win32 Capture vs `.SHD` Shadow Files

Direct binary reads of `C:\Windows\System32\spool\PRINTERS\*.SHD` were rejected during Day 3 empirical testing because `spoolsv.exe` (`NT AUTHORITY\SYSTEM`) enforces exclusive ACLs and file locks (`[Errno 13] Permission denied` even from an elevated Admin shell). Instead, Printust combines two non-admin Win32 streams inside `printust_watcher`:

- **Stream A — Live Queue Hook ([spooler_hook.rs](../watcher/src/spooler_hook.rs)):** `OpenPrinterW` + `FindFirstPrinterChangeNotification` (`PRINTER_CHANGE_ADD_JOB | PRINTER_CHANGE_SET_JOB`) + `WaitForMultipleObjects` + two-pass `GetJobW` Level 2 (`JOB_INFO_2W`) reads `pDevMode` (`dmDuplex`, `dmCopies`, `dmColor`, `dmPaperSize`), `TotalPages`, and unmasked `pDocument` without Admin rights, guarding every `pDevMode` dereference with non-null, alignment, buffer-bounds, and `dmSize` checks.
- **Stream B — Ground Truth Event Subscription ([event_sub.rs](../watcher/src/event_sub.rs)):** Native `EvtSubscribe` push stream on `Microsoft-Windows-PrintService/Operational` filtered to Events `805` (`RenderJobDiag`: `Copies`, `Color`) and `307` (`DocumentPrinted`: actual printed `PagesPrinted`, `JobId`, `PrinterName`, `EventRecordID`).

---

## 2. How Bounded `mpsc::SyncSender` + `try_send` Prevents Deadlocks

Inside Windows `wevtapi.dll`, `EvtClose(subscription)` **blocks synchronously until every active push callback returns**. Calling SQLite (or even blocking `SyncSender::send`) inside `evt_subscribe_callback` risks a multi-thread shutdown deadlock if the consumer stalls on a full queue during `WatcherHandle::stop()`.

- **Callback Isolation:** `evt_subscribe_callback` wraps its body in `std::panic::catch_unwind(AssertUnwindSafe(...))` (never unwinding across FFI), touches **zero SQLite connections**, and uses **non-blocking `try_send(...)`** onto a bounded `mpsc::sync_channel`.
- **Ordered Teardown:** `EventLogSubscription::drop` always invokes `EvtClose(subscription)` *before* `drop(Box::from_raw(ctx_ptr))` so OS threadpool callbacks drain cleanly before freeing `EvtCallbackContext`.
- **Supervisor Loop ([src-tauri/src/lib.rs](../src-tauri/src/lib.rs)):** The Tauri background supervisor drains `WatcherOutput` events, commits them sequentially into `Arc<Mutex<SessionEngine>>`, emits `"printust://session-updated"`, and automatically restarts `WatcherService` in live future-events mode if `EvtSubscribeActionError` signals a live log clear (`wevtutil cl`).

---

## 3. Crash-Safe Exactly-Once Billing (`watcher_bookmarks` + `UNIQUE` Dedup)

If bookmark saving and bill updates ran in separate SQLite transactions, a crash milliseconds in between would either leave a stale bookmark (double-billing on restart) or skip an unbilled job.

- **Single Atomic Transaction ([core/src/db.rs](../core/src/db.rs)):** `LedgerDb::ingest_job_and_bookmark_atomic` checks/inserts the job into `print_jobs`, opens/increments `sessions.total_amount_paise`, and upserts `watcher_bookmarks(channel_name, bookmark_xml, updated_at)` inside **one WAL transaction**.
- **Engine-Level Idempotency:** `idx_print_jobs_printer_event_record` enforces `UNIQUE(printer_name, event_record_id) WHERE event_record_id IS NOT NULL`, returning `IngestOutcome::DuplicateIgnored` if a bookmark replay or rollback re-delivers a billed event—even after closing the session.

---

## 4. Top 6 Viva Questions & Answers Mapped to Code

1. **Why can `DuplexMode::Unknown` still appear on an unpaused printer, and how is it billed?**  
   Fast sub-50ms jobs on unpaused queues can finish and leave the spooler before `GetJobW` Level 2 runs (`ERROR_INVALID_PARAMETER` / `ERROR_FILE_NOT_FOUND` in [spooler_hook.rs](../watcher/src/spooler_hook.rs)). [models.rs](../core/src/models.rs) (`resolved_is_duplex` / `resolved_anomaly_flag`) bills safely at simplex rates, tags `DEVMODE_RACE_MISSED`, and triggers the amber warning pill in [App.tsx](../ui/src/App.tsx).
2. **Why does [correlator.rs](../watcher/src/correlator.rs) never multiply `PagesPrinted * Copies` on Chrome fallback?**  
   Chrome/Edge unrolls copies into GDI before spooling so Event `307` `PagesPrinted` already equals `doc_pages * copies` (`3p x 3c -> 9`). `IncomingPrintJob::resolved_pricing_inputs` in [models.rs](../core/src/models.rs) only splits `(raw_pages, raw_copies)` when `raw_pages * copies == effective_pages` or a confirmed duplex job scales evenly by `copies`.
3. **What happens if the shop owner clears Event Log (`wevtutil cl`) offline vs live?**  
   At startup, [event_sub.rs](../watcher/src/event_sub.rs) catches `EvtCreateBookmark` / `EvtSubscribeStartAfterBookmark` failure and falls back to `EvtSubscribeToFutureEvents` with a warning. While running live, `EvtSubscribeActionError` triggers the supervisor in [src-tauri/src/lib.rs](../src-tauri/src/lib.rs) to restart `WatcherService` with `initial_bookmark = None`.
4. **Where is the production SQLite database stored and why?**  
   [src-tauri/src/lib.rs](../src-tauri/src/lib.rs) opens `app.path().app_data_dir()/printust.db` (per-user writable AppData), avoiding `Program Files` UAC read-only failures and keeping runtime `.db` / `-wal` / `-shm` artifacts out of the source repo.
5. **How is currency precision guaranteed across Rust and React?**  
   All prices, session totals, and add-ons use `i64` paise (`Rs. 1 = 100 paise`) in [pricing.rs](../core/src/pricing.rs) and SQLite `INTEGER` columns. On the frontend, [money.ts](../ui/src/utils/money.ts) formats and parses rupees purely via integer division, modulo, and string splitting (`formatPaiseToInr`, `parseRupeesToPaise`) with zero floating-point arithmetic.
6. **Why does `WatcherHandle::stop_inner` close `EvtSubscribe` and signal `ShutdownEvent` before joining threads?**  
   Signaling `ShutdownEvent` unblocks `WaitForMultipleObjects` in [spooler_hook.rs](../watcher/src/spooler_hook.rs) in `<1ms`, while dropping `EventLogSubscription` closes `EvtSubscribe` cleanly before reclaiming `ctx_ptr`, and `send_output_cancellable` in [watcher/src/lib.rs](../watcher/src/lib.rs) prevents the consumer thread from stalling on a full UI channel.
