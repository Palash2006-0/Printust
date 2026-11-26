# PrintLedger Spike Findings & Locked Capture Strategy (Phase 1 -> Phase 3)

Decision document summarizing Phase 1 (Days 1–3) Windows Print Spooler empirical findings against `ShopTest_PCL6` (`HP Universal Printing PCL 6`, port `NUL`) and locking the architecture for the Phase 3 Rust capture engine.

---

## 1. Days 1–2 Summary (Event ID `307` & `805` + Driver Lie Matrix)

From live Event Log captures (`JobId 3`–`10`) documented in [driver_lie_matrix.md](driver_lie_matrix.md):

- **Event `805` (`RenderJobDiag`):** Emits `JobId`, `GdiJobSize`, `ICMMethod`, `Color` (`1` = Monochrome/B&W, `2` = Color), `XRes`, `YRes`, `Quality`, `Copies`, `TTOption` — and **omits Duplex completely**.
- **Event `307` (`DocumentPrinted`):** Emits `Param1` (`JobId`), `Param2` (`DocumentName` — Chromium sends generic `"Print Document"`), `Param5` (`ShopTest_PCL6`), `Param6` (`NUL`), `Param7` (`SizeBytes`), `Param8` (`PagesPrinted`).
- **Observed Multi-Copy & Duplex Quirks (`HP Universal Printing PCL 6` + Chrome):**
  - On multi-copy jobs (`JobId 5`, `JobId 9`, `JobId 10`), `PagesPrinted` in Event `307` **already reflects unrolled impressions** (`doc_pages * copies`, e.g., `3 pages × 3 copies -> PagesPrinted=9`). Blindly multiplying `PagesPrinted * Copies` would overbill.
  - On Duplex (`JobId 7`: 4-page 2-sided PDF), Event `307` reported `PagesPrinted=4` (logical sides) instead of `2` physical sheets, with zero Duplex indication in Event `805` or `307`.

---

## 2. Day 3 Duplex Verification (`JOB_INFO_2` vs `.SHD` Shadow Files)

Live probe executed via `.\spike\.venv\Scripts\python.exe .\spike\shd_devmode_probe.py` in Administrator PowerShell against a **Paused** `ShopTest_PCL6` queue (`Paused=True`):

| Test Job Sent to Paused `ShopTest_PCL6` | `JOB_INFO_2` (`TotalPages` / `dmCopies` / `dmDuplex` / `dmColor`) | `.SHD` (`C:\Windows\System32\spool\PRINTERS`) | `JOB_INFO_2` Sufficient? | Observed Notes |
| :--- | :--- | :--- | :---: | :--- |
| **`JobId=11`:** 4-page PDF × 3 copies (Duplex Long-Edge, B&W) | `Doc='test_doc.pdf'`, `User='<USER>'`, `TotalPages=4`, `Size=8744176`, `Copies=3`, `Duplex=2 -> DUPLEX_VERTICAL (Long-Edge)`, `Color=1 -> MONOCHROME`, `PaperSize=1 -> Letter` | `PermissionError` (`[Errno 13]` on `FP00003.SHD` — locked by `spoolsv.exe` / SYSTEM ACL even as Admin) | **Yes** | **Critical findings:** `JOB_INFO_2.pDevMode` exposed `Duplex=2` (`DUPLEX_VERTICAL`), `Copies=3`, `Color=1`, and unmasked `pDocument` without elevation, while `TotalPages=4` reported per-copy document pages (`4 pages × 3 copies ≈ 8.74 MB`). |
| **`JobId=12`:** 2-page PDF × 1 copy (Simplex, Color) | `Doc='test_doc.pdf'`, `User='<USER>'`, `TotalPages=2`, `Size=775456`, `Copies=1`, `Duplex=1 -> SIMPLEX`, `Color=2 -> COLOR`, `PaperSize=1 -> Letter` | `PermissionError` (`[Errno 13]` on `FP00003.SHD` — locked by `spoolsv.exe` / SYSTEM ACL even as Admin) | **Yes** | Confirmed `Duplex=1 -> SIMPLEX` and `Color=2 -> COLOR` alongside `TotalPages=2` and unmasked `pDocument`. |

### Locked Answers to Day 3 Architectural Questions
1. **Does `win32print.EnumJobs` Level 2 (`JOB_INFO_2.pDevMode`) expose `dmDuplex`, `dmCopies`, and `dmColor` accurately without Administrator rights?**
   **Yes.** Tested both non-elevated and elevated: `JOB_INFO_2.pDevMode` exposes `Copies`, `Duplex` (`1` Simplex, `2` Long-Edge, `3` Short-Edge), `Color` (`1` Monochrome, `2` Color), `PaperSize`, `TotalPages`, and the real document title `pDocument` (unlike Event `307`'s masked `"Print Document"`).
2. **Is raw `.SHD` binary parsing needed in Phase 3 Rust, or is `JOB_INFO_2` sufficient?**
   **`JOB_INFO_2` is sufficient — raw `.SHD` file reading should NOT be used.** Even in an Administrator PowerShell, direct file reads under `C:\Windows\System32\spool\PRINTERS\` hit `[Errno 13] Permission denied` (`FP00003.SHD` protected by `spoolsv.exe` lock / SYSTEM ACL). Relying on Win32 Spooler API (`EnumJobsW` Level 2) works reliably as a standard non-admin desktop user.

---

## 3. Locked Phase 3 Rust Capture Strategy

1. **Queue / Spool Hook (`JOB_INFO_2` via `FindFirstPrinterChangeNotification` + `GetJobW` Level 2):**
   - Runs as a non-elevated background thread inside the Tauri/Rust daemon (`windows-rs`).
   - Uses event-driven `FindFirstPrinterChangeNotification` + `GetJobW` Level 2 on the notified `JobId` (not periodic `EnumJobs` polling) to capture `DEVMODEW` attributes (`dmDuplex`, `dmCopies`, `dmColor`, `dmPaperSize`, `dmFields`), `TotalPages`, and unmasked `pDocument` title while the job is spooling/queued.
   - Stores per-`windows_job_id` metadata in a bounded cache with TTL eviction (`15s`+) and immediate eviction upon Event `307` correlation.
2. **Completion Anchor (Windows Event Log `EvtSubscribe` + Persistent XML Bookmark):**
   - Subscribes via `EvtSubscribe` (`EvtSubscribeStartAfterBookmark`, XML bookmark persisted in SQLite `WAL`) to `Microsoft-Windows-PrintService/Operational` for Event `307` (`DocumentPrinted`) and Event `805` (`RenderJobDiag`).
   - Joins queued `JOB_INFO_2` metadata (`duplex`, `dmCopies`, `dmColor`, real `doc_name`) with Event `307`/`805` on `windows_job_id`:
     - **Total sides:** Use Event `307` `PagesPrinted` (or `TotalPages * dmCopies` cross-check; flag `driver_anomaly_flag` if they disagree).
     - **Physical sheets:** If `duplex` is `DUPLEX_VERTICAL` or `DUPLEX_HORIZONTAL` (`dmDuplex in {2, 3}`), compute `full_duplex_sheets = sides / 2` and `remaining_single_sheets = sides % 2` (matching `pricing.rs` Day 4 math).

### Paused vs. Live Race Condition & Graceful Fallback
- **Paused vs. Unpaused Timing Caveat:** Day 3 `JOB_INFO_2` validation ran with `Paused=True`. On an unpaused fast printer (`NUL` port or high-speed USB/network laser printers), small jobs can spool and exit the queue in `<50ms` before `GetJobW` reads them. Live unpaused timing must be verified in Phase 3 using `FindFirstPrinterChangeNotification` + `GetJobW` Level 2 on the notified `JobId` (never periodic `EnumJobs` polling).
- **`duplex = Unknown` Fallback Contract:** If `pDevMode` is `None`, `DM_DUPLEX` (`0x00001000`) is unset in `dmFields`, or the job exits the queue before `GetJobW` completes:
  - Classify `duplex = Unknown` (do **not** silently treat it as verified simplex).
  - Bill at the **simplex** rate as the safe default, set `driver_anomaly_flag = "DUPLEX_UNKNOWN"` (truthy flag in SQLite `print_jobs`), and surface a non-blocking indicator in the Mini-HUD so the operator can verify without losing or blocking the job.
