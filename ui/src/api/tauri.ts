import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type SessionStatus = "ACTIVE" | "PAID_UPI" | "PAID_CASH" | "KHATA" | "VOID";

export type JamInkKind = "BW" | "COLOR";

export type DuplexMode = "SIMPLEX" | "DUPLEX_LONG_EDGE" | "DUPLEX_SHORT_EDGE" | "UNKNOWN";

export type ColorMode = "MONOCHROME" | "COLOR" | "UNKNOWN";

export interface SessionJobBreakdown {
  job_id: number;
  doc_name: string;
  effective_pages: number;
  duplex_mode: DuplexMode;
  color_mode: ColorMode;
  is_duplex: boolean;
  is_color: boolean;
  cost_paise: number;
  anomaly: string | null;
  is_unverified: boolean;
}

export interface SessionSummary {
  session_id: number;
  status: SessionStatus;
  started_at: string;
  closed_at: string | null;
  job_count: number;
  bw_pages: number;
  color_pages: number;
  jam_deducted_pages: number;
  bw_jam_deducted_pages: number;
  color_jam_deducted_pages: number;
  bw_jam_rate_paise: number;
  color_jam_rate_paise: number;
  addons_total_paise: number;
  total_amount_paise: number;
  has_anomaly: boolean;
  unverified_jobs_count: number;
  anomaly_reasons: string[];
  jobs: SessionJobBreakdown[];
  last_doc_name: string | null;
}

export interface DailyAuditReport {
  date_prefix: string;
  total_spooler_pages: number;
  billed_pages: number;
  jam_deducted_pages: number;
  void_or_unbilled_pages: number;
  upi_total_paise: number;
  cash_total_paise: number;
  khata_total_paise: number;
  discrepancy_flag: boolean;
}

export interface PricingRules {
  bw_single_paise: number;
  bw_duplex_sheet_paise: number;
  color_single_paise: number;
  color_duplex_sheet_paise: number;
  bw_bulk_threshold_sides: number;
  bw_bulk_side_paise: number;
}

export interface WatcherStatus {
  running: boolean;
  printer_name: string;
  last_error: string | null;
  last_captured_at: string | null;
  restart_count: number;
}

export const isTauriRuntime = (): boolean =>
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

export const getActiveSession = (): Promise<SessionSummary | null> =>
  invoke<SessionSummary | null>("get_active_session");

export const applyJamDeduction = (kind: JamInkKind): Promise<SessionSummary | null> =>
  invoke<SessionSummary | null>("apply_jam_deduction", { kind });

export const addSessionAddon = (
  label: string,
  qty: number,
  unitPricePaise: number,
): Promise<SessionSummary | null> =>
  invoke<SessionSummary | null>("add_session_addon", {
    label,
    qty,
    unitPricePaise,
  });

export const closeActiveSession = (
  status: Exclude<SessionStatus, "ACTIVE">,
): Promise<SessionSummary | null> =>
  invoke<SessionSummary | null>("close_active_session", { status });

export const getDailyAudit = (datePrefix: string): Promise<DailyAuditReport> =>
  invoke<DailyAuditReport>("get_daily_audit", { datePrefix });

export const getPricingRules = (): Promise<PricingRules> =>
  invoke<PricingRules>("get_pricing_rules");

export const updatePricingRules = (rules: PricingRules): Promise<void> =>
  invoke<void>("update_pricing_rules", { rules });

export const getWatcherStatus = (): Promise<WatcherStatus> =>
  invoke<WatcherStatus>("get_watcher_status");

export const onSessionUpdated = (
  handler: (summary: SessionSummary) => void,
): Promise<UnlistenFn> =>
  listen<SessionSummary>("printust://session-updated", (event) => handler(event.payload));

export const onWatcherStatusChanged = (
  handler: (status: WatcherStatus) => void,
): Promise<UnlistenFn> =>
  listen<WatcherStatus>("printust://watcher-status", (event) => handler(event.payload));
