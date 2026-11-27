# `printust_core` Architecture & Study Notes (Days 4–5)

Concise post-exam reference mapping every module inside `core/` (`printust_core`) to its responsibilities, safety invariants, and interview defense points.

---

## 1. Crate Layout (`core/src/`)

| Module | Primary Types & Entry Points | Responsibility |
| :--- | :--- | :--- |
| [lib.rs](../core/src/lib.rs) | Re-exports `LedgerDb`, `SessionEngine`, `calculate_job_cost_paise`, `models::*` | Pure Rust domain crate decoupled from Tauri, UI, and Win32 FFI. |
| [models.rs](../core/src/models.rs) | `SessionStatus`, `PricingRules`, `IncomingPrintJob`, `SessionAddon`, `SessionSummary`, `DailyAuditReport`, `LedgerError` | Typed domain schema, `SCREAMING_SNAKE_CASE` SQLite/Serde mappings, rate validation, and `thiserror` error propagation. |
| [pricing.rs](../core/src/pricing.rs) | `calculate_job_cost_paise(pages_per_copy, copies, is_color, is_duplex, rules)` | Integer `i64` paise slab calculation: B&W bulk slab (`>= 50` sides), per-copy duplex sheet math, and odd-page trailing single-sheet fallback. |
| [db.rs](../core/src/db.rs) | `LedgerDb` (`open_in_memory`, `open_file`, `append_print_job`, `add_session_addon`, `deduct_jam_page`, `close_session`, `get_session_summary`, `get_daily_audit_report`) | SQLite `WAL` persistence, `CHECK (rate_paise >= 0)` rule table, atomic transactions, append-only `print_jobs` audit trail, and EOD discrepancy SQL queries. |
| [session.rs](../core/src/session.rs) | `SessionEngine` (`ingest_print_job`, `apply_jam_deduction`, `add_addon`, `close_active_session`, `get_active_summary`, `get_daily_audit`) | High-level billing state machine facade consumed by Tauri IPC commands and the Day 8 spooler watcher. |

---

## 2. Core Engineering Decisions

1. **Integer `i64` Paise (`Rs. 1 = 100 paise`):**
   Never uses `f32`/`f64` for money, eliminating binary floating-point drift across thousands of prints and SQLite `SUM(...)` aggregations. Arithmetic uses `i64::from(u32)` lossless widening + `saturating_mul` / `saturating_add` (or `checked_mul` for add-ons) so corrupted driver counts can never overflow into negative bills or panic.
2. **Per-Copy Duplex Sheet Physics (`pricing.rs`):**
   On a real printer, Copy 2's page 1 always starts on a fresh sheet. `calculate_job_cost_paise` computes `(pages_per_copy / 2) * duplex_rate + (pages_per_copy % 2) * single_rate` **per copy**, then multiplies by `copies`—avoiding Rs. 1/copy underbilling on odd-page multi-copy jobs (e.g., `3 pages × 2 copies B&W duplex = 1000 paise`, not `900`). Meanwhile, `SessionEngine::resolve_pricing_inputs` safely handles unrolled Event `307` counts when `GetJobW` missed a sub-50ms job so pages are never double-multiplied.
3. **SQLite `WAL` + `foreign_keys` + `busy_timeout` (`db.rs`):**
   Every opened connection immediately applies `PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;`. `WAL` preserves committed sessions across sudden shop power cuts and lets read queries run concurrently with writes; `busy_timeout = 5000` avoids spurious `SQLITE_BUSY` failures if UI reads overlap watcher writes.
4. **Append-Only `print_jobs` + Atomic Transactions:**
   Spooler jobs are inserted atomically alongside `sessions.total_amount_paise` inside `conn.transaction()?` and are never deleted or overwritten—even if the operator voids a session or deducts jammed pages (`jam_deducted_pages`), giving the shop owner a tamper-evident EOD audit trail.
5. **Zero `.unwrap()` / `.expect()` in `core/src/`:**
   `LedgerError` derives `thiserror::Error` with `#[from] rusqlite::Error` and `#[from] serde_json::Error`, letting every fallible operation propagate cleanly via `?` without panicking the background daemon.

---

## 3. Quick Interview Q&As Mapped to Code

1. **Why store currency as `i64` paise instead of `f64` rupees?**  
   Decimal values like `Rs. 1.50` or bulk totals drift under IEEE-754 binary floats (`0.1 + 0.2 != 0.3`). Storing `i64` paise in Rust (`PricingRules`, `calculate_job_cost_paise`) and `INTEGER` in SQLite (`total_amount_paise`) makes billing exact and SQL `SUM()` deterministic.
2. **How do you prevent double-counting or underbilling when Event `307` unrolls copies?**  
   In `SessionEngine::resolve_pricing_inputs`, if `GetJobW` captured per-copy `TotalPages` (`raw_pages * raw_copies == effective_pages`), we pass `(raw_pages, raw_copies)` to `calculate_job_cost_paise` so per-copy duplex math stays exact; if `GetJobW` missed the job and only unrolled Event `307` `PagesPrinted` exists (`effective_pages == raw_pages` while `raw_copies > 1`), we pass `(effective_pages, 1)` so total sides are never multiplied twice.
3. **How do you prevent half-written bills during a shop power cut?**  
   `LedgerDb::init_connection` enables `PRAGMA journal_mode = WAL;` and `append_print_job` / `add_session_addon` / `deduct_jam_page` wrap both the detail insert and the `sessions.total_amount_paise` update inside an atomic `rusqlite::Transaction`. Either both commit or both roll back.
4. **How does the Owner EOD Audit catch unbilled prints or cash theft?**  
   `print_jobs` is append-only (captures every OS spooler event). `LedgerDb::get_daily_audit_report` sums `total_spooler_pages` from `print_jobs` and compares against `billed_pages` (`PAID_UPI`, `PAID_CASH`, `KHATA`), `jam_deducted_pages`, and `void_or_unbilled_pages` (`VOID`, `ACTIVE`, or orphan rows), setting `discrepancy_flag = true` whenever unbilled pages exist.
5. **Why keep `printust_core` separate from `src-tauri`?**  
   Keeping domain math, SQLite transactions, and state transitions in pure Rust without Tauri/Win32 UI coupling lets all 15+ unit/integration tests run in `<10ms` in isolation via `cargo test` with zero GUI/OS side effects.
