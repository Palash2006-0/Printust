//! Deterministic correlation between Stream A (`JOB_INFO_2W`) and Stream B (Events `805` + `307`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use printust_core::{ColorMode, DuplexMode, IncomingPrintJob};

use crate::xml_parser::{Event307Data, Event805Data, ParsedSpoolerEvent};

/// Retention window for Stream A `DEVMODEW` snapshots awaiting Event `307`.
pub const DEVMODE_CACHE_TTL: Duration = Duration::from_secs(600);

/// Retention window for unmatched Event `805` records awaiting Event `307`.
pub const PENDING_805_TTL: Duration = Duration::from_secs(15);

/// Brief hold window when Event `307` arrives slightly before Event `805`.
pub const LATE_805_WAIT_TTL: Duration = Duration::from_millis(500);

/// Maximum `EventRecordID` distance allowed when pairing an `805` and `307` event.
pub const LATE_805_MAX_RECORD_GAP: i64 = 2;

/// Stream A snapshot captured via `GetJobW` Level 2 (`JOB_INFO_2W` + `pDevMode`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedDevMode {
    pub printer_name: String,
    pub job_id: u32,
    pub doc_name: String,
    pub total_pages: u32,
    pub copies: Option<u32>,
    pub duplex_mode: DuplexMode,
    pub color_mode: ColorMode,
    pub paper_size: Option<i16>,
}

#[derive(Debug, Clone)]
struct DevModeEntry {
    data: CachedDevMode,
    captured_at: Instant,
}

#[derive(Debug, Clone)]
struct Pending805Entry {
    event: Event805Data,
    received_at: Instant,
}

#[derive(Debug, Clone)]
struct Pending307Entry {
    event: Event307Data,
    bookmark_xml: Option<String>,
    received_at: Instant,
}

/// Fully reconciled print job ready for `printust_core` ingestion plus optional latest bookmark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelatedJob {
    pub job: IncomingPrintJob,
    pub bookmark_xml: Option<String>,
    pub used_late_805: bool,
}

/// Correlates Stream A (`JOB_INFO_2W` devmode cache) and Stream B (Events `805` & `307`).
#[derive(Debug, Default)]
pub struct JobCorrelator {
    devmode_cache: HashMap<(String, u32), DevModeEntry>,
    pending_805: HashMap<u32, Pending805Entry>,
    pending_307: HashMap<u32, Pending307Entry>,
}

fn normalize_printer_key(printer_name: &str) -> String {
    printer_name.trim().to_ascii_lowercase()
}

fn record_gap_within_limit(rec_a: i64, rec_b: i64) -> bool {
    (rec_a - rec_b).abs() <= LATE_805_MAX_RECORD_GAP
}

impl JobCorrelator {
    /// Construct an empty correlator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of currently cached Stream A `DEVMODEW` entries.
    pub fn devmode_cache_len(&self) -> usize {
        self.devmode_cache.len()
    }

    /// Number of currently cached unmatched Event `805` entries.
    pub fn pending_805_len(&self) -> usize {
        self.pending_805.len()
    }

    /// Number of Event `307` entries currently awaiting a possible late Event `805`.
    pub fn pending_307_len(&self) -> usize {
        self.pending_307.len()
    }

    /// Store or refresh a Stream A `CachedDevMode` snapshot using the current wall-clock `Instant`.
    pub fn record_devmode(&mut self, devmode: CachedDevMode) {
        self.record_devmode_at(devmode, Instant::now());
    }

    /// Deterministic version of `record_devmode` accepting an explicit timestamp for testing.
    pub fn record_devmode_at(&mut self, devmode: CachedDevMode, now: Instant) {
        self.prune_devmode_and_805(now);
        let key = (normalize_printer_key(&devmode.printer_name), devmode.job_id);
        let merged = if let Some(existing) = self.devmode_cache.get(&key) {
            merge_devmode(&existing.data, devmode)
        } else {
            devmode
        };
        self.devmode_cache.insert(
            key,
            DevModeEntry {
                data: merged,
                captured_at: now,
            },
        );
    }

    /// Process a parsed Event `805` or `307` and emit any newly completed `CorrelatedJob` records.
    pub fn process_spooler_event(
        &mut self,
        event: ParsedSpoolerEvent,
        bookmark_xml: Option<String>,
    ) -> Vec<CorrelatedJob> {
        self.process_spooler_event_at(event, bookmark_xml, Instant::now())
    }

    /// Deterministic version of `process_spooler_event` accepting an explicit timestamp.
    pub fn process_spooler_event_at(
        &mut self,
        event: ParsedSpoolerEvent,
        bookmark_xml: Option<String>,
        now: Instant,
    ) -> Vec<CorrelatedJob> {
        let mut emitted = self.drain_expired_at(now);

        match event {
            ParsedSpoolerEvent::RenderDiag805(ev805) => {
                if let Some(p307) = self.pending_307.get(&ev805.job_id) {
                    if record_gap_within_limit(ev805.record_id, p307.event.record_id) {
                        if let Some(removed_307) = self.pending_307.remove(&ev805.job_id) {
                            let devmode = self.take_devmode(&removed_307.event.printer_name, ev805.job_id);
                            let latest_bookmark = bookmark_xml.or(removed_307.bookmark_xml);
                            let job = reconcile_job(removed_307.event, Some(ev805), devmode);
                            emitted.push(CorrelatedJob {
                                job,
                                bookmark_xml: latest_bookmark,
                                used_late_805: true,
                            });
                            return emitted;
                        }
                    }
                }

                // Store/replace unmatched 805 for up to 15 seconds.
                self.pending_805.insert(
                    ev805.job_id,
                    Pending805Entry {
                        event: ev805,
                        received_at: now,
                    },
                );
            }
            ParsedSpoolerEvent::DocumentPrinted307(ev307) => {
                // If an unmatched older 307 shared this recycled JobId, flush it before overwriting.
                if let Some(previous_307) = self.pending_307.remove(&ev307.job_id) {
                    let prev_dm = self.take_devmode(&previous_307.event.printer_name, previous_307.event.job_id);
                    let prev_job = reconcile_job(previous_307.event, None, prev_dm);
                    emitted.push(CorrelatedJob {
                        job: prev_job,
                        bookmark_xml: previous_307.bookmark_xml,
                        used_late_805: false,
                    });
                }

                let has_matching_805 = self
                    .pending_805
                    .get(&ev307.job_id)
                    .is_some_and(|entry| record_gap_within_limit(ev307.record_id, entry.event.record_id));
                let matched_805 = if has_matching_805 {
                    self.pending_805
                        .remove(&ev307.job_id)
                        .map(|entry| entry.event)
                } else {
                    None
                };

                if let Some(ev805) = matched_805 {
                    let devmode = self.take_devmode(&ev307.printer_name, ev307.job_id);
                    let job = reconcile_job(ev307, Some(ev805), devmode);
                    emitted.push(CorrelatedJob {
                        job,
                        bookmark_xml,
                        used_late_805: false,
                    });
                } else {
                    // Hold briefly in case Event 805 arrives within LATE_805_WAIT_TTL (<= 500ms).
                    self.pending_307.insert(
                        ev307.job_id,
                        Pending307Entry {
                            event: ev307,
                            bookmark_xml,
                            received_at: now,
                        },
                    );
                }
            }
        }

        emitted
    }

    /// Flush expired caches (`devmode` > 10m, `pending_805` > 15s) and emit any Event `307`
    /// records whose late-`805` grace period (`500ms`) elapsed without a matching `805`.
    pub fn drain_expired_at(&mut self, now: Instant) -> Vec<CorrelatedJob> {
        self.prune_devmode_and_805(now);

        let expired_307_ids: Vec<u32> = self
            .pending_307
            .iter()
            .filter_map(|(&job_id, entry)| {
                (now.saturating_duration_since(entry.received_at) >= LATE_805_WAIT_TTL)
                    .then_some(job_id)
            })
            .collect();

        let mut flushed = Vec::with_capacity(expired_307_ids.len());
        for job_id in expired_307_ids {
            if let Some(entry) = self.pending_307.remove(&job_id) {
                let devmode = self.take_devmode(&entry.event.printer_name, job_id);
                let job = reconcile_job(entry.event, None, devmode);
                flushed.push(CorrelatedJob {
                    job,
                    bookmark_xml: entry.bookmark_xml,
                    used_late_805: false,
                });
            }
        }
        flushed.sort_by_key(|c| c.job.event_record_id.unwrap_or(0));
        flushed
    }

    fn prune_devmode_and_805(&mut self, now: Instant) {
        self.devmode_cache
            .retain(|_, entry| now.saturating_duration_since(entry.captured_at) <= DEVMODE_CACHE_TTL);
        self.pending_805
            .retain(|_, entry| now.saturating_duration_since(entry.received_at) <= PENDING_805_TTL);
    }

    fn take_devmode(&mut self, printer_name: &str, job_id: u32) -> Option<CachedDevMode> {
        let key = (normalize_printer_key(printer_name), job_id);
        self.devmode_cache.remove(&key).map(|entry| entry.data)
    }
}

fn merge_devmode(existing: &CachedDevMode, incoming: CachedDevMode) -> CachedDevMode {
    CachedDevMode {
        printer_name: incoming.printer_name,
        job_id: incoming.job_id,
        doc_name: if incoming.doc_name.trim().is_empty() {
            existing.doc_name.clone()
        } else {
            incoming.doc_name
        },
        total_pages: if incoming.total_pages > 0 {
            incoming.total_pages
        } else {
            existing.total_pages
        },
        copies: incoming.copies.or(existing.copies),
        duplex_mode: if incoming.duplex_mode == DuplexMode::Unknown {
            existing.duplex_mode
        } else {
            incoming.duplex_mode
        },
        color_mode: if incoming.color_mode == ColorMode::Unknown {
            existing.color_mode
        } else {
            incoming.color_mode
        },
        paper_size: incoming.paper_size.or(existing.paper_size),
    }
}

fn reconcile_job(
    ev307: Event307Data,
    ev805: Option<Event805Data>,
    devmode: Option<CachedDevMode>,
) -> IncomingPrintJob {
    // Prefer unmasked JOB_INFO_2W.pDocument when Chromium replaces the filename with "Print Document".
    let doc_name = match devmode.as_ref().map(|dm| dm.doc_name.trim()) {
        Some(dm_doc)
            if !dm_doc.is_empty()
                && (ev307.doc_name.trim().is_empty()
                    || ev307.doc_name.trim() == "Print Document") =>
        {
            dm_doc.to_owned()
        }
        _ if !ev307.doc_name.trim().is_empty() => ev307.doc_name.trim().to_owned(),
        _ => "Untitled Print".to_owned(),
    };

    let color_mode = match ev805.as_ref().map(|e| e.color) {
        Some(c) if c != ColorMode::Unknown => c,
        _ => devmode
            .as_ref()
            .map(|dm| dm.color_mode)
            .unwrap_or(ColorMode::Unknown),
    };
    let is_color = color_mode.is_color_billable();

    let duplex_mode = devmode
        .as_ref()
        .map(|dm| dm.duplex_mode)
        .unwrap_or(DuplexMode::Unknown);
    let is_duplex = duplex_mode.is_duplex_billable();

    let raw_copies = ev805
        .as_ref()
        .map(|e| e.copies)
        .filter(|&c| c >= 1)
        .or_else(|| devmode.as_ref().and_then(|dm| dm.copies).filter(|&c| c >= 1))
        .unwrap_or(1);

    let effective_pages = ev307.pages_printed;
    let dm_total_pages = devmode.as_ref().map(|dm| dm.total_pages).unwrap_or(0);

    // Driver Lie Matrix protection:
    // - If JOB_INFO_2.TotalPages * copies == Event 307 PagesPrinted, record per-copy raw_pages
    //   so multi-copy odd-duplex computes sheets cleanly per copy.
    // - Otherwise Chrome/Edge already unrolled copies into PagesPrinted, so preserve raw_pages =
    //   effective_pages (SessionEngine will never double-multiply).
    let raw_pages = if dm_total_pages > 0 && dm_total_pages.saturating_mul(raw_copies) == effective_pages
    {
        dm_total_pages
    } else {
        effective_pages
    };

    let driver_anomaly_flag = if duplex_mode == DuplexMode::Unknown {
        Some("DEVMODE_RACE_MISSED".to_owned())
    } else if ev805.is_none() {
        Some("EVENT_805_MISSING".to_owned())
    } else if raw_copies > 1 && effective_pages < raw_copies {
        Some("PAGE_COPY_MISMATCH".to_owned())
    } else {
        None
    };

    IncomingPrintJob {
        windows_job_id: ev307.job_id,
        event_record_id: Some(ev307.record_id),
        printer_name: ev307.printer_name,
        doc_name,
        raw_pages,
        raw_copies,
        effective_pages,
        is_duplex,
        is_color,
        duplex_mode,
        color_mode,
        driver_anomaly_flag,
        captured_at: ev307.timestamp,
    }
}
