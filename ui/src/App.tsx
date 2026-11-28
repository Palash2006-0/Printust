import { useCallback, useEffect, useState } from "react";
import {
  addSessionAddon,
  applyJamDeduction,
  closeActiveSession,
  getActiveSession,
  getDailyAudit,
  getPricingRules,
  getWatcherStatus,
  isTauriRuntime,
  onSessionUpdated,
  onWatcherStatusChanged,
  updatePricingRules,
  type DailyAuditReport,
  type JamInkKind,
  type PricingRules,
  type SessionJobBreakdown,
  type SessionStatus,
  type SessionSummary,
  type WatcherStatus,
} from "./api/tauri";
import { formatPaiseToInr, parseRupeesToPaise } from "./utils/money";

function todayUtcPrefix(): string {
  return new Date().toISOString().slice(0, 10);
}

function formatJobInk(job: SessionJobBreakdown): string {
  if (job.color_mode === "COLOR") return "Color";
  if (job.color_mode === "MONOCHROME") return "B&W";
  return job.is_color ? "Color?" : "B&W?";
}

function formatJobDuplex(job: SessionJobBreakdown): string {
  if (job.duplex_mode === "DUPLEX_LONG_EDGE" || job.duplex_mode === "DUPLEX_SHORT_EDGE") {
    return "2-side";
  }
  if (job.duplex_mode === "SIMPLEX") {
    return "1-side";
  }
  return "Duplex?";
}

export default function App() {
  const [summary, setSummary] = useState<SessionSummary | null>(null);
  const [watcher, setWatcher] = useState<WatcherStatus>({
    running: false,
    printer_name: "ShopTest_PCL6",
    last_error: null,
    last_captured_at: null,
    restart_count: 0,
  });
  const [view, setView] = useState<"counter" | "audit">("counter");
  const [busy, setBusy] = useState(false);
  const [errorBanner, setErrorBanner] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  // Custom add-on inline drawer state
  const [showCustomAddon, setShowCustomAddon] = useState(false);
  const [customLabel, setCustomLabel] = useState("");
  const [customRupees, setCustomRupees] = useState("");

  // Daily audit & rate configuration drawer state
  const [auditDate, setAuditDate] = useState(todayUtcPrefix);
  const [audit, setAudit] = useState<DailyAuditReport | null>(null);
  const [rules, setRules] = useState<PricingRules | null>(null);
  const [rulesOpen, setRulesOpen] = useState(false);

  const loadInitialState = useCallback(async () => {
    if (!isTauriRuntime()) {
      setErrorBanner(
        "Browser preview mode: Tauri IPC is unavailable in a browser tab. Run 'npm --prefix ui run tauri dev' in your PowerShell window to launch the Printust desktop app.",
      );
      return;
    }
    try {
      const [active, status, loadedRules] = await Promise.all([
        getActiveSession(),
        getWatcherStatus(),
        getPricingRules(),
      ]);
      setSummary(active);
      setWatcher(status);
      setRules(loadedRules);
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    }
  }, []);

  const refreshAudit = useCallback(async (dateStr: string) => {
    if (!isTauriRuntime()) return;
    try {
      const [report, loadedRules] = await Promise.all([
        getDailyAudit(dateStr),
        getPricingRules(),
      ]);
      setAudit(report);
      setRules(loadedRules);
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    }
  }, []);

  useEffect(() => {
    void loadInitialState();
    if (!isTauriRuntime()) return;

    let mounted = true;
    let unlistenSession: (() => void) | undefined;
    let unlistenStatus: (() => void) | undefined;

    void onSessionUpdated((next) => {
      if (mounted) {
        setSummary(next);
        setNotice(null);
        setErrorBanner(null);
      }
    })
      .then((fn) => {
        if (mounted) {
          unlistenSession = fn;
        } else {
          fn();
        }
      })
      .catch((err) => {
        if (mounted) setErrorBanner(String(err));
      });

    void onWatcherStatusChanged((nextStatus) => {
      if (mounted) {
        setWatcher(nextStatus);
      }
    })
      .then((fn) => {
        if (mounted) {
          unlistenStatus = fn;
        } else {
          fn();
        }
      })
      .catch((err) => {
        if (mounted) setErrorBanner(String(err));
      });

    return () => {
      mounted = false;
      unlistenSession?.();
      unlistenStatus?.();
    };
  }, [loadInitialState]);

  useEffect(() => {
    if (view === "audit") {
      void refreshAudit(auditDate);
    }
  }, [view, auditDate, refreshAudit]);

  const handleJam = async (kind: JamInkKind) => {
    if (!summary || busy) return;
    setBusy(true);
    try {
      const updated = await applyJamDeduction(kind);
      setSummary(updated);
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handlePresetAddon = async (label: string, unitPricePaise: number) => {
    if (!summary || busy) return;
    setBusy(true);
    try {
      const updated = await addSessionAddon(label, 1, unitPricePaise);
      setSummary(updated);
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleCustomAddonSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!summary || busy) return;
    const paise = parseRupeesToPaise(customRupees);
    if (!customLabel.trim() || paise === null) {
      setErrorBanner("Enter a valid add-on label and rupee amount (e.g. 15 or 12.50)");
      return;
    }
    setBusy(true);
    try {
      const updated = await addSessionAddon(customLabel.trim(), 1, paise);
      setSummary(updated);
      setCustomLabel("");
      setCustomRupees("");
      setShowCustomAddon(false);
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleCloseSession = async (status: Exclude<SessionStatus, "ACTIVE">) => {
    if (!summary || busy) return;
    setBusy(true);
    try {
      const closed = await closeActiveSession(status);
      setSummary(null);
      setShowCustomAddon(false);
      if (closed) {
        setNotice(
          `Closed #${closed.session_id} (${closed.status}) — ${formatPaiseToInr(closed.total_amount_paise)}`,
        );
      }
      setErrorBanner(null);
    } catch (err) {
      setErrorBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleSaveRules = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!rules || busy) return;
    setBusy(true);
    try {
      await updatePricingRules(rules);
      const refreshed = await getActiveSession();
      setSummary(refreshed);
      setNotice("Pricing rules saved");
      setErrorBanner(null);
      setRulesOpen(false);
    } catch (err) {
      setErrorBanner(String(err));
    } finally {
      setBusy(false);
    }
  };

  const totalPaise = summary?.total_amount_paise ?? 0;
  const bwJamPaise = summary?.bw_jam_rate_paise ?? rules?.bw_single_paise ?? 200;
  const colorJamPaise = summary?.color_jam_rate_paise ?? rules?.color_single_paise ?? 1000;
  const canJamBw = Boolean(summary) && !busy && (summary?.bw_pages ?? 0) > 0;
  const canJamColor = Boolean(summary) && !busy && (summary?.color_pages ?? 0) > 0;
  const unverifiedCount = summary?.unverified_jobs_count ?? (summary?.has_anomaly ? 1 : 0);

  return (
    <div className="min-h-screen flex flex-col bg-zinc-950 text-zinc-50 p-3 gap-2 text-xs">
      {/* Top Status Strip */}
      <header className="flex items-center justify-between gap-2 border-b border-zinc-800 pb-2">
        <div
          className="flex items-center gap-2 min-w-0"
          title={watcher.last_error ?? `Watching ${watcher.printer_name}`}
        >
          <span
            className={`h-2.5 w-2.5 rounded-full shrink-0 ${
              watcher.running && !watcher.last_error
                ? "bg-emerald-400 animate-pulse"
                : watcher.running
                  ? "bg-amber-400"
                  : "bg-rose-500"
            }`}
          />
          <span className="truncate font-medium text-zinc-200">
            {watcher.running ? `Watching: ${watcher.printer_name}` : `Offline: ${watcher.printer_name}`}
          </span>
        </div>

        <button
          type="button"
          onClick={() => setView((prev) => (prev === "counter" ? "audit" : "counter"))}
          className="px-2.5 py-1 rounded bg-zinc-800 hover:bg-zinc-700 text-zinc-100 font-medium border border-zinc-700 shrink-0 cursor-pointer"
        >
          {view === "counter" ? "EOD Audit" : "Counter"}
        </button>
      </header>

      {(errorBanner || watcher.last_error) && (
        <div className="px-2.5 py-1.5 rounded bg-rose-950/70 border border-rose-700/80 text-rose-200 text-[11px] break-words">
          {errorBanner ?? watcher.last_error}
        </div>
      )}

      {notice && (
        <div className="px-2.5 py-1.5 rounded bg-emerald-950/70 border border-emerald-700/70 text-emerald-200 text-[11px] flex items-center justify-between gap-2">
          <span className="truncate">{notice}</span>
          <button
            type="button"
            onClick={() => setNotice(null)}
            className="text-emerald-300 hover:text-white cursor-pointer"
          >
            ✕
          </button>
        </div>
      )}

      {view === "counter" ? (
        <main className="flex-1 flex flex-col justify-between gap-2">
          {/* Primary Bill Display */}
          <section className="rounded-lg bg-zinc-900 border border-zinc-800 p-2.5 flex flex-col gap-2">
            <div className="flex items-center justify-between text-[11px] text-zinc-400">
              <span>{summary ? `Session #${summary.session_id}` : "Ready for Next Customer"}</span>
              <span className="uppercase tracking-wide font-semibold text-emerald-400">
                {summary ? summary.status : "IDLE"}
              </span>
            </div>

            <div className="text-4xl font-bold tracking-tight tabular-nums text-white leading-none py-0.5">
              {formatPaiseToInr(totalPaise)}
            </div>

            {(unverifiedCount > 0 || summary?.has_anomaly) && (
              <div className="px-2.5 py-1.5 rounded bg-amber-500/20 border border-amber-400/80 text-amber-100 text-[11px] font-semibold flex items-center justify-between gap-2">
                <span>
                  ⚠ Verify Pages/Duplex ({unverifiedCount}{" "}
                  {unverifiedCount === 1 ? "job" : "jobs"})
                </span>
                {summary && summary.anomaly_reasons.length > 0 && (
                  <span className="text-[10px] font-mono text-amber-300 truncate">
                    {summary.anomaly_reasons.join(", ")}
                  </span>
                )}
              </div>
            )}

            {/* Compact Breakdown */}
            <div className="grid grid-cols-5 gap-1.5 pt-2 border-t border-zinc-800 text-center">
              <div className="bg-zinc-950/70 rounded p-1.5">
                <div className="text-[10px] text-zinc-400">B&amp;W</div>
                <div className="font-semibold tabular-nums">{summary?.bw_pages ?? 0}</div>
              </div>
              <div className="bg-zinc-950/70 rounded p-1.5">
                <div className="text-[10px] text-zinc-400">Color</div>
                <div className="font-semibold tabular-nums text-sky-300">
                  {summary?.color_pages ?? 0}
                </div>
              </div>
              <div className="bg-zinc-950/70 rounded p-1.5">
                <div className="text-[10px] text-zinc-400">Jobs</div>
                <div className="font-semibold tabular-nums">{summary?.job_count ?? 0}</div>
              </div>
              <div className="bg-zinc-950/70 rounded p-1.5">
                <div className="text-[10px] text-zinc-400">Jam -</div>
                <div className="font-semibold tabular-nums text-amber-300">
                  {summary?.jam_deducted_pages ?? 0}
                </div>
                {summary && summary.jam_deducted_pages > 0 && (
                  <div className="text-[9px] text-zinc-400 tabular-nums leading-none mt-0.5">
                    {summary.bw_jam_deducted_pages}B/{summary.color_jam_deducted_pages}C
                  </div>
                )}
              </div>
              <div className="bg-zinc-950/70 rounded p-1.5">
                <div className="text-[10px] text-zinc-400">Add-ons</div>
                <div className="font-semibold tabular-nums">
                  {formatPaiseToInr(summary?.addons_total_paise ?? 0)}
                </div>
              </div>
            </div>

            {/* Scannable Active Jobs Breakdown */}
            {summary && summary.jobs.length > 0 ? (
              <div className="flex flex-col gap-1 max-h-24 overflow-y-auto pr-0.5 border-t border-zinc-800/80 pt-1.5">
                {summary.jobs.map((job) => (
                  <div
                    key={job.job_id}
                    className={`flex items-center justify-between gap-2 text-[11px] leading-tight rounded px-1.5 py-1 ${
                      job.is_unverified
                        ? "bg-amber-950/50 border border-amber-500/60 text-amber-100"
                        : "bg-zinc-950/60 text-zinc-200"
                    }`}
                    title={job.anomaly ? `${job.doc_name} (${job.anomaly})` : job.doc_name}
                  >
                    <span className="truncate min-w-0 font-medium">
                      {job.is_unverified ? "⚠ " : ""}
                      {job.doc_name}
                    </span>
                    <span className="shrink-0 tabular-nums text-[10px] text-zinc-300 font-medium">
                      {job.effective_pages}p {formatJobInk(job)} • {formatJobDuplex(job)} •{" "}
                      <span className="text-zinc-100 font-semibold">
                        {formatPaiseToInr(job.cost_paise)}
                      </span>
                    </span>
                  </div>
                ))}
              </div>
            ) : (
              <div className="text-[11px] text-zinc-400 truncate border-t border-zinc-800/80 pt-1.5">
                Doc:{" "}
                <span className="text-zinc-200">
                  {summary?.last_doc_name ?? "Waiting for print job..."}
                </span>
              </div>
            )}
          </section>

          {/* 1-Click Ink-Scoped Jam & Add-on Actions */}
          <section className="flex flex-col gap-1.5">
            <div className="grid grid-cols-2 gap-1.5">
              <button
                type="button"
                disabled={!canJamBw}
                onClick={() => void handleJam("BW")}
                className="py-2 px-2 rounded bg-amber-950/80 hover:bg-amber-900/80 disabled:opacity-40 border border-amber-700/70 text-amber-100 font-semibold tabular-nums cursor-pointer disabled:cursor-not-allowed"
              >
                -1 B&amp;W Jam • {formatPaiseToInr(bwJamPaise)}
              </button>
              <button
                type="button"
                disabled={!canJamColor}
                onClick={() => void handleJam("COLOR")}
                className="py-2 px-2 rounded bg-sky-950/80 hover:bg-sky-900/80 disabled:opacity-40 border border-sky-700/70 text-sky-100 font-semibold tabular-nums cursor-pointer disabled:cursor-not-allowed"
              >
                -1 Color Jam • {formatPaiseToInr(colorJamPaise)}
              </button>
            </div>

            <div className="grid grid-cols-3 gap-1.5">
              <button
                type="button"
                disabled={!summary || busy}
                onClick={() => void handlePresetAddon("Spiral Binding", 3000)}
                className="py-1.5 px-1.5 rounded bg-zinc-800 hover:bg-zinc-700 disabled:opacity-40 border border-zinc-700 text-zinc-100 font-semibold cursor-pointer disabled:cursor-not-allowed"
              >
                + Spiral ₹30
              </button>
              <button
                type="button"
                disabled={!summary || busy}
                onClick={() => void handlePresetAddon("Lamination", 2000)}
                className="py-1.5 px-1 rounded bg-zinc-800 hover:bg-zinc-700 disabled:opacity-40 border border-zinc-700 text-zinc-100 text-[11px] font-semibold cursor-pointer disabled:cursor-not-allowed"
              >
                + Lamination ₹20
              </button>
              <button
                type="button"
                disabled={!summary || busy}
                onClick={() => setShowCustomAddon((prev) => !prev)}
                className="py-1.5 px-1.5 rounded bg-zinc-800 hover:bg-zinc-700 disabled:opacity-40 border border-zinc-700 text-zinc-100 font-semibold cursor-pointer disabled:cursor-not-allowed"
              >
                + Custom
              </button>
            </div>

            {showCustomAddon && summary && (
              <form onSubmit={handleCustomAddonSubmit} className="flex gap-1.5">
                <input
                  type="text"
                  placeholder="Item (e.g. Folder)"
                  value={customLabel}
                  onChange={(e) => setCustomLabel(e.target.value)}
                  className="flex-1 min-w-0 px-2 py-1.5 rounded bg-zinc-900 border border-zinc-700 text-zinc-100 placeholder-zinc-500"
                />
                <input
                  type="text"
                  placeholder="₹15"
                  value={customRupees}
                  onChange={(e) => setCustomRupees(e.target.value)}
                  className="w-20 px-2 py-1.5 rounded bg-zinc-900 border border-zinc-700 text-zinc-100 placeholder-zinc-500 tabular-nums"
                />
                <button
                  type="submit"
                  disabled={busy}
                  className="px-3 py-1.5 rounded bg-emerald-600 hover:bg-emerald-500 text-white font-semibold cursor-pointer"
                >
                  Add
                </button>
              </form>
            )}
          </section>

          {/* 1-Click Close Bar */}
          <section className="grid grid-cols-4 gap-1.5 pt-1 border-t border-zinc-800">
            <button
              type="button"
              disabled={!summary || busy}
              onClick={() => void handleCloseSession("PAID_UPI")}
              className="py-2.5 rounded bg-emerald-600 hover:bg-emerald-500 disabled:opacity-40 text-white font-bold cursor-pointer disabled:cursor-not-allowed"
            >
              Paid UPI
            </button>
            <button
              type="button"
              disabled={!summary || busy}
              onClick={() => void handleCloseSession("PAID_CASH")}
              className="py-2.5 rounded bg-sky-600 hover:bg-sky-500 disabled:opacity-40 text-white font-bold cursor-pointer disabled:cursor-not-allowed"
            >
              Paid Cash
            </button>
            <button
              type="button"
              disabled={!summary || busy}
              onClick={() => void handleCloseSession("KHATA")}
              className="py-2.5 rounded bg-amber-600 hover:bg-amber-500 disabled:opacity-40 text-white font-bold cursor-pointer disabled:cursor-not-allowed"
            >
              Khata
            </button>
            <button
              type="button"
              disabled={!summary || busy}
              onClick={() => void handleCloseSession("VOID")}
              className="py-2.5 rounded bg-rose-700 hover:bg-rose-600 disabled:opacity-40 text-white font-bold cursor-pointer disabled:cursor-not-allowed"
            >
              Void
            </button>
          </section>
        </main>
      ) : (
        <main className="flex-1 flex flex-col gap-2.5 overflow-y-auto">
          {/* EOD Audit Drawer */}
          <div className="flex items-center gap-2">
            <input
              type="date"
              value={auditDate}
              onChange={(e) => setAuditDate(e.target.value)}
              className="flex-1 px-2 py-1.5 rounded bg-zinc-900 border border-zinc-700 text-zinc-100"
            />
            <button
              type="button"
              onClick={() => void refreshAudit(auditDate)}
              className="px-3 py-1.5 rounded bg-zinc-800 hover:bg-zinc-700 border border-zinc-700 font-semibold cursor-pointer"
            >
              Refresh
            </button>
          </div>

          {audit && (
            <>
              <section className="grid grid-cols-3 gap-2">
                <div className="rounded bg-zinc-900 border border-zinc-800 p-2.5">
                  <div className="text-[10px] text-zinc-400">UPI</div>
                  <div className="text-sm font-bold tabular-nums text-emerald-400">
                    {formatPaiseToInr(audit.upi_total_paise)}
                  </div>
                </div>
                <div className="rounded bg-zinc-900 border border-zinc-800 p-2.5">
                  <div className="text-[10px] text-zinc-400">Cash</div>
                  <div className="text-sm font-bold tabular-nums text-sky-400">
                    {formatPaiseToInr(audit.cash_total_paise)}
                  </div>
                </div>
                <div className="rounded bg-zinc-900 border border-zinc-800 p-2.5">
                  <div className="text-[10px] text-zinc-400">Khata</div>
                  <div className="text-sm font-bold tabular-nums text-amber-400">
                    {formatPaiseToInr(audit.khata_total_paise)}
                  </div>
                </div>
              </section>

              <section className="rounded bg-zinc-900 border border-zinc-800 p-3 flex flex-col gap-2">
                <div className="flex justify-between">
                  <span className="text-zinc-400">Total OS Spooler Pages</span>
                  <span className="font-semibold tabular-nums">{audit.total_spooler_pages}</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-zinc-400">Billed Pages</span>
                  <span className="font-semibold tabular-nums text-emerald-300">
                    {audit.billed_pages}
                  </span>
                </div>
                <div className="flex justify-between">
                  <span className="text-zinc-400">Jam Deducted Pages</span>
                  <span className="font-semibold tabular-nums text-amber-300">
                    {audit.jam_deducted_pages}
                  </span>
                </div>
                <div className="flex justify-between">
                  <span className="text-zinc-400">Void / Unbilled Pages</span>
                  <span className="font-semibold tabular-nums text-rose-400">
                    {audit.void_or_unbilled_pages}
                  </span>
                </div>

                {audit.discrepancy_flag ? (
                  <div className="mt-1 px-2.5 py-1.5 rounded bg-rose-950/70 border border-rose-700 text-rose-200 font-medium">
                    ⚠ Discrepancy detected: unbilled/VOID pages exist for {audit.date_prefix}
                  </div>
                ) : (
                  <div className="mt-1 px-2.5 py-1.5 rounded bg-emerald-950/60 border border-emerald-700/70 text-emerald-200 font-medium">
                    ✓ All captured spooler pages reconciled
                  </div>
                )}
              </section>
            </>
          )}

          {/* Compact Owner Rate Slabs (Paise) */}
          <section className="rounded bg-zinc-900 border border-zinc-800 p-2.5">
            <button
              type="button"
              onClick={() => setRulesOpen((prev) => !prev)}
              className="w-full flex items-center justify-between font-semibold text-zinc-200 cursor-pointer"
            >
              <span>Rate Slabs (Paise)</span>
              <span>{rulesOpen ? "▲" : "▼"}</span>
            </button>

            {rulesOpen && rules && (
              <form onSubmit={handleSaveRules} className="mt-2 grid grid-cols-2 gap-2">
                {(
                  [
                    ["bw_single_paise", "B&W 1-Side (p)"],
                    ["bw_duplex_sheet_paise", "B&W Duplex (p)"],
                    ["color_single_paise", "Color 1-Side (p)"],
                    ["color_duplex_sheet_paise", "Color Duplex (p)"],
                    ["bw_bulk_threshold_sides", "Bulk Min Sides"],
                    ["bw_bulk_side_paise", "Bulk B&W (p)"],
                  ] as const
                ).map(([field, label]) => (
                  <label key={field} className="flex flex-col gap-0.5 text-[10px] text-zinc-400">
                    {label}
                    <input
                      type="number"
                      min={0}
                      value={rules[field]}
                      onChange={(e) => {
                        const val = Number.parseInt(e.target.value, 10);
                        setRules((prev) =>
                          prev ? { ...prev, [field]: Number.isFinite(val) ? val : 0 } : prev,
                        );
                      }}
                      className="px-2 py-1 rounded bg-zinc-950 border border-zinc-700 text-zinc-100 tabular-nums"
                    />
                  </label>
                ))}
                <button
                  type="submit"
                  disabled={busy}
                  className="col-span-2 py-1.5 rounded bg-emerald-600 hover:bg-emerald-500 text-white font-semibold cursor-pointer"
                >
                  Save Rates
                </button>
              </form>
            )}
          </section>
        </main>
      )}
    </div>
  );
}
