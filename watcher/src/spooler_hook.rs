//! Stream A: Event-driven Win32 print queue watcher (`FindFirstPrinterChangeNotification` + `GetJobW` Level 2).

use std::collections::HashSet;
use std::ffi::c_void;
use std::mem::{align_of, size_of};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;

use printust_core::{ColorMode, DuplexMode};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER,
    ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE, WAIT_EVENT, WAIT_FAILED, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DMCOLOR_COLOR, DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL, DMDUP_SIMPLEX, DMDUP_VERTICAL,
    DM_COLOR, DM_COPIES, DM_DUPLEX, DM_PAPERSIZE,
};
use windows::Win32::Graphics::Printing::{
    ClosePrinter, EnumJobsW, FindClosePrinterChangeNotification,
    FindFirstPrinterChangeNotification, FindNextPrinterChangeNotification, FreePrinterNotifyInfo,
    GetJobW, OpenPrinterW, JOB_INFO_2W, JOB_NOTIFY_FIELD_DEVMODE, JOB_NOTIFY_FIELD_DOCUMENT,
    JOB_NOTIFY_FIELD_STATUS, JOB_NOTIFY_TYPE, PRINTER_CHANGE_ADD_JOB, PRINTER_CHANGE_SET_JOB,
    PRINTER_NOTIFY_INFO, PRINTER_NOTIFY_INFO_DATA, PRINTER_NOTIFY_INFO_DISCARDED,
    PRINTER_NOTIFY_OPTIONS, PRINTER_NOTIFY_OPTIONS_REFRESH, PRINTER_NOTIFY_OPTIONS_TYPE,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForMultipleObjects, INFINITE};

use crate::correlator::CachedDevMode;
use crate::xml_parser::WatcherError;
use crate::StreamEvent;

/// Send/Sync wrapper around an owned manual-reset Win32 event `HANDLE` used for clean shutdown.
#[derive(Debug)]
pub(crate) struct ShutdownEvent {
    raw: isize,
}

impl ShutdownEvent {
    pub(crate) fn new_manual_reset() -> Result<Self, WatcherError> {
        // SAFETY: Null security attributes & name with manual-reset=true, initial-state=false.
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.map_err(|err| {
            WatcherError::Win32 {
                api: "CreateEventW",
                message: err.to_string(),
            }
        })?;
        Ok(Self {
            raw: handle.0 as isize,
        })
    }

    pub(crate) fn signal(&self) -> Result<(), WatcherError> {
        let handle = self.as_handle();
        if handle.is_invalid() {
            return Ok(());
        }
        // SAFETY: `self.raw` was returned by `CreateEventW` and remains open until `Drop`.
        unsafe { SetEvent(handle) }.map_err(|err| WatcherError::Win32 {
            api: "SetEvent",
            message: err.to_string(),
        })
    }

    pub(crate) fn as_handle(&self) -> HANDLE {
        HANDLE(self.raw as *mut c_void)
    }
}

impl Drop for ShutdownEvent {
    fn drop(&mut self) {
        let handle = self.as_handle();
        if !handle.is_invalid() {
            // SAFETY: Owned handle from `CreateEventW`; closed exactly once on Drop.
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

struct PrinterHandleGuard(HANDLE);

impl Drop for PrinterHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Handle obtained from `OpenPrinterW`; closed once on Drop.
            unsafe {
                let _ = ClosePrinter(self.0);
            }
        }
    }
}

struct ChangeNotifyGuard(HANDLE);

impl Drop for ChangeNotifyGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Handle obtained from `FindFirstPrinterChangeNotification`; closed once on Drop.
            unsafe {
                let _ = FindClosePrinterChangeNotification(self.0);
            }
        }
    }
}

struct NotifyInfoGuard(*const PRINTER_NOTIFY_INFO);

impl Drop for NotifyInfoGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Pointer allocated by `FindNextPrinterChangeNotification`; freed once on Drop.
            unsafe {
                let _ = FreePrinterNotifyInfo(self.0);
            }
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a NUL-terminated UTF-16 string only if `ptr` points within `[buf_start .. buf_end)`.
unsafe fn read_pwstr_within_bounds(ptr: PWSTR, buf_start: usize, buf_end: usize) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let addr = ptr.0 as usize;
    if addr < buf_start || addr >= buf_end || !addr.is_multiple_of(align_of::<u16>()) {
        return None;
    }
    let max_units = (buf_end - addr) / size_of::<u16>();
    if max_units == 0 {
        return None;
    }
    // SAFETY: Caller verified `[addr .. buf_end)` lies strictly inside the allocated `GetJobW` buffer.
    let slice = unsafe { std::slice::from_raw_parts(ptr.0, max_units) };
    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
    Some(String::from_utf16_lossy(&slice[..len]).trim().to_owned())
}

/// Validate `JOB_INFO_2W` + `pDevMode` pointer invariants and copy all fields into an owned
/// `CachedDevMode` before the `GetJobW` buffer is freed.
///
/// Exported as crate-internal helper so offline unit tests can exercise all pointer/bounds guards
/// using crafted byte buffers without requiring a live Windows print queue.
pub fn extract_devmode_from_job_info2_buf(
    printer_name: &str,
    buf: &[u8],
) -> Option<CachedDevMode> {
    let buf_start = buf.as_ptr() as usize;
    let buf_end = buf_start.checked_add(buf.len())?;
    if buf.len() < size_of::<JOB_INFO_2W>() || !buf_start.is_multiple_of(align_of::<JOB_INFO_2W>())
    {
        return None;
    }

    // SAFETY: Buffer length >= size_of::<JOB_INFO_2W>() and alignment was checked immediately above.
    let job_info = unsafe { &*buf.as_ptr().cast::<JOB_INFO_2W>() };
    let job_id = job_info.JobId;
    let total_pages = job_info.TotalPages;
    // SAFETY: `read_pwstr_within_bounds` verifies bounds & alignment inside `[buf_start .. buf_end)`.
    let doc_name =
        unsafe { read_pwstr_within_bounds(job_info.pDocument, buf_start, buf_end) }.unwrap_or_default();

    // Guard 1: Non-null `pDevMode` check.
    let p_dm = job_info.pDevMode;
    if p_dm.is_null() {
        return Some(CachedDevMode {
            printer_name: printer_name.to_owned(),
            job_id,
            doc_name,
            total_pages,
            copies: None,
            duplex_mode: DuplexMode::Unknown,
            color_mode: ColorMode::Unknown,
            paper_size: None,
        });
    }

    // Guard 2: Alignment check for `DEVMODEW`.
    let dm_addr = p_dm as usize;
    if !dm_addr.is_multiple_of(align_of::<DEVMODEW>()) {
        return None;
    }

    // Guard 3: Minimum struct footprint strictly inside `[buf_start .. buf_end)`.
    let min_dm_size = size_of::<DEVMODEW>();
    if dm_addr < buf_start || dm_addr.checked_add(min_dm_size).is_none_or(|end| end > buf_end) {
        return None;
    }

    // SAFETY: `p_dm` is non-null, aligned for `DEVMODEW`, and covers `size_of::<DEVMODEW>()` bytes
    // within `[buf_start .. buf_end)`.
    let dm = unsafe { &*p_dm };

    // Guard 4: Declared `dmSize` must cover `DEVMODEW` and stay inside `[buf_start .. buf_end)`.
    let declared_size = usize::from(dm.dmSize);
    if declared_size < min_dm_size
        || dm_addr
            .checked_add(declared_size)
            .is_none_or(|end| end > buf_end)
    {
        return None;
    }

    let dm_fields = dm.dmFields;

    let duplex_mode = if (dm_fields & DM_DUPLEX).0 != 0 {
        match dm.dmDuplex {
            DMDUP_SIMPLEX => DuplexMode::Simplex,
            DMDUP_VERTICAL => DuplexMode::DuplexLongEdge,
            DMDUP_HORIZONTAL => DuplexMode::DuplexShortEdge,
            _ => DuplexMode::Unknown,
        }
    } else {
        DuplexMode::Unknown
    };

    let color_mode = if (dm_fields & DM_COLOR).0 != 0 {
        match dm.dmColor {
            DMCOLOR_MONOCHROME => ColorMode::Monochrome,
            DMCOLOR_COLOR => ColorMode::Color,
            _ => ColorMode::Unknown,
        }
    } else {
        ColorMode::Unknown
    };

    let copies = if (dm_fields & DM_COPIES).0 != 0 {
        // SAFETY: Active union variant `Anonymous1.Anonymous1` carries `dmCopies` & `dmPaperSize`
        // for printer DEVMODEW structs when `DM_COPIES` / `DM_PAPERSIZE` are flagged.
        let val = unsafe { dm.Anonymous1.Anonymous1.dmCopies };
        u32::try_from(val).ok().filter(|&c| c >= 1)
    } else {
        None
    };

    let paper_size = if (dm_fields & DM_PAPERSIZE).0 != 0 {
        // SAFETY: Same printer DEVMODEW union member (`dmPaperSize`).
        Some(unsafe { dm.Anonymous1.Anonymous1.dmPaperSize })
    } else {
        None
    };

    Some(CachedDevMode {
        printer_name: printer_name.to_owned(),
        job_id,
        doc_name,
        total_pages,
        copies,
        duplex_mode,
        color_mode,
        paper_size,
    })
}

fn query_job_devmode(
    hprinter: HANDLE,
    printer_name: &str,
    job_id: u32,
) -> Result<Option<CachedDevMode>, WatcherError> {
    let mut needed: u32 = 0;

    // Pass 1: Determine exact buffer size required for `JOB_INFO_2W` + strings + `DEVMODEW`.
    // SAFETY: `hprinter` is open; `pjob = None` queries `pcbneeded` cleanly.
    let first_ok = unsafe { GetJobW(hprinter, job_id, 2, None, &mut needed) };
    if !first_ok.as_bool() {
        // SAFETY: Reads thread-local Win32 last error immediately after `GetJobW`.
        let err = unsafe { GetLastError() };
        if err == ERROR_INVALID_PARAMETER || err == ERROR_FILE_NOT_FOUND {
            // Fast unpaused printer already flushed this job before Stream A read it.
            return Ok(None);
        }
        if err != ERROR_INSUFFICIENT_BUFFER && err != ERROR_MORE_DATA {
            return Err(WatcherError::Win32 {
                api: "GetJobW(size)",
                message: format!("Win32 error code {}", err.0),
            });
        }
    }

    if needed == 0 {
        return Ok(None);
    }

    // Allocate u64-backed buffer so start address is guaranteed >= 8-byte aligned for JOB_INFO_2W.
    let word_len = (needed as usize).div_ceil(size_of::<u64>());
    let mut aligned_words = vec![0u64; word_len];
    let byte_len = needed as usize;
    // SAFETY: `aligned_words` owns `word_len * 8 >= byte_len` initialized bytes.
    let buf: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(aligned_words.as_mut_ptr().cast::<u8>(), byte_len) };

    let mut returned: u32 = 0;
    // Pass 2: Populate aligned buffer and extract owned fields immediately before freeing.
    // SAFETY: `hprinter` is valid and `buf` has exact length `needed`.
    let second_ok = unsafe { GetJobW(hprinter, job_id, 2, Some(buf), &mut returned) };
    if !second_ok.as_bool() {
        // SAFETY: Reads thread-local Win32 last error immediately after `GetJobW`.
        let err = unsafe { GetLastError() };
        if err == ERROR_INVALID_PARAMETER || err == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        return Err(WatcherError::Win32 {
            api: "GetJobW(data)",
            message: format!("Win32 error code {}", err.0),
        });
    }

    let used_len = (returned as usize).min(byte_len);
    Ok(extract_devmode_from_job_info2_buf(
        printer_name,
        &buf[..used_len],
    ))
}

fn enum_queued_job_ids(hprinter: HANDLE) -> Result<Vec<u32>, WatcherError> {
    let mut needed: u32 = 0;
    let mut returned: u32 = 0;

    // SAFETY: Two-call `EnumJobsW` sizing with Level 2 for up to 64 queued jobs.
    let first_res = unsafe { EnumJobsW(hprinter, 0, 64, 2, None, &mut needed, &mut returned) };
    if let Err(err) = first_res {
        if err.code() != windows::core::HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) {
            return Err(WatcherError::Win32 {
                api: "EnumJobsW(size)",
                message: err.to_string(),
            });
        }
    }

    if needed == 0 {
        return Ok(Vec::new());
    }

    let word_len = (needed as usize).div_ceil(size_of::<u64>());
    let mut aligned_words = vec![0u64; word_len];
    let byte_len = needed as usize;
    // SAFETY: `aligned_words` owns `word_len * 8 >= byte_len` initialized bytes.
    let buf: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(aligned_words.as_mut_ptr().cast::<u8>(), byte_len) };

    // SAFETY: Valid printer handle and aligned buffer of length `needed`.
    unsafe { EnumJobsW(hprinter, 0, 64, 2, Some(buf), &mut needed, &mut returned) }.map_err(
        |err| WatcherError::Win32 {
            api: "EnumJobsW(data)",
            message: err.to_string(),
        },
    )?;

    let max_entries = buf.len() / size_of::<JOB_INFO_2W>();
    let count = (returned as usize).min(max_entries);
    if count == 0 {
        return Ok(Vec::new());
    }

    // SAFETY: Buffer starts aligned for `JOB_INFO_2W` and holds at least `count` contiguous structs.
    let entries = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<JOB_INFO_2W>(), count) };
    Ok(entries.iter().map(|j| j.JobId).collect())
}

fn collect_notified_job_ids(
    change_handle: HANDLE,
    hprinter: HANDLE,
    refresh_options_ptr: *const c_void,
) -> Result<Vec<u32>, WatcherError> {
    let mut pdw_change: u32 = 0;
    let mut p_notify_raw: *mut c_void = std::ptr::null_mut();

    // SAFETY: Valid change notification handle from `FindFirstPrinterChangeNotification`.
    let ok = unsafe {
        FindNextPrinterChangeNotification(
            change_handle,
            Some(&mut pdw_change),
            None,
            Some(&mut p_notify_raw),
        )
    };
    if !ok.as_bool() {
        // SAFETY: Reads Win32 error immediately after failure.
        let err = unsafe { GetLastError() };
        return Err(WatcherError::Win32 {
            api: "FindNextPrinterChangeNotification",
            message: format!("Win32 error code {}", err.0),
        });
    }

    let _guard = NotifyInfoGuard(p_notify_raw.cast::<PRINTER_NOTIFY_INFO>());
    if p_notify_raw.is_null() {
        return enum_queued_job_ids(hprinter);
    }

    // SAFETY: Spooler returned a valid `PRINTER_NOTIFY_INFO` pointer when call succeeded non-null.
    let info = unsafe { &*p_notify_raw.cast::<PRINTER_NOTIFY_INFO>() };

    let mut seen = HashSet::new();
    let mut ids = Vec::new();

    if (info.Flags & PRINTER_NOTIFY_INFO_DISCARDED) != 0 {
        // Refresh spooler notification tracking and sweep active queue via EnumJobsW.
        let mut refresh_change: u32 = 0;
        let mut refreshed_raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: `refresh_options_ptr` points to valid `PRINTER_NOTIFY_OPTIONS` with REFRESH flag.
        let _ = unsafe {
            FindNextPrinterChangeNotification(
                change_handle,
                Some(&mut refresh_change),
                Some(refresh_options_ptr),
                Some(&mut refreshed_raw),
            )
        };
        let _refreshed_guard = NotifyInfoGuard(refreshed_raw.cast::<PRINTER_NOTIFY_INFO>());
        return enum_queued_job_ids(hprinter);
    }

    let count = info.Count as usize;
    if count > 0 {
        // SAFETY: `PRINTER_NOTIFY_INFO.aData` is a C variable-length array of `Count` items.
        let data_ptr = std::ptr::addr_of!(info.aData).cast::<PRINTER_NOTIFY_INFO_DATA>();
        // SAFETY: `count` items are guaranteed initialized by `winspool.drv` when non-discarded.
        let items = unsafe { std::slice::from_raw_parts(data_ptr, count) };
        for item in items {
            if u32::from(item.Type) == JOB_NOTIFY_TYPE && seen.insert(item.Id) {
                ids.push(item.Id);
            }
        }
    }

    if ids.is_empty() && (pdw_change & (PRINTER_CHANGE_ADD_JOB | PRINTER_CHANGE_SET_JOB)) != 0 {
        return enum_queued_job_ids(hprinter);
    }
    Ok(ids)
}

fn try_send_devmode(tx: &SyncSender<StreamEvent>, dm: CachedDevMode) -> bool {
    match tx.try_send(StreamEvent::DevModeCaptured(dm)) {
        Ok(()) => true,
        Err(TrySendError::Disconnected(_)) => false,
        Err(TrySendError::Full(_)) => {
            let _ = tx.try_send(StreamEvent::Error(WatcherError::Win32 {
                api: "spooler_hook",
                message: "event stream queue full; dropped DevMode snapshot".to_owned(),
            }));
            true
        }
    }
}

/// Run the blocking Stream A spooler hook loop until `shutdown` is signaled.
pub(crate) fn run_spooler_hook(
    printer_name: &str,
    shutdown_event: Arc<ShutdownEvent>,
    shutdown_flag: Arc<AtomicBool>,
    tx: SyncSender<StreamEvent>,
) -> Result<(), WatcherError> {
    let wide_printer = to_wide_null(printer_name);
    let mut raw_hprinter = HANDLE::default();

    // SAFETY: Null-terminated UTF-16 printer name and valid output `HANDLE`.
    unsafe { OpenPrinterW(PCWSTR(wide_printer.as_ptr()), &mut raw_hprinter, None) }.map_err(
        |err| WatcherError::Win32 {
            api: "OpenPrinterW",
            message: err.to_string(),
        },
    )?;
    let printer_guard = PrinterHandleGuard(raw_hprinter);

    let mut job_fields: [u16; 3] = [
        JOB_NOTIFY_FIELD_STATUS as u16,
        JOB_NOTIFY_FIELD_DOCUMENT as u16,
        JOB_NOTIFY_FIELD_DEVMODE as u16,
    ];
    let mut notify_types = [PRINTER_NOTIFY_OPTIONS_TYPE {
        Type: JOB_NOTIFY_TYPE as u16,
        Reserved0: 0,
        Reserved1: 0,
        Reserved2: 0,
        Count: job_fields.len() as u32,
        pFields: job_fields.as_mut_ptr(),
    }];
    let options = PRINTER_NOTIFY_OPTIONS {
        Version: 2,
        Flags: 0,
        Count: notify_types.len() as u32,
        pTypes: notify_types.as_mut_ptr(),
    };
    let refresh_options = PRINTER_NOTIFY_OPTIONS {
        Version: 2,
        Flags: PRINTER_NOTIFY_OPTIONS_REFRESH,
        Count: notify_types.len() as u32,
        pTypes: notify_types.as_mut_ptr(),
    };

    // SAFETY: Open printer handle + valid `PRINTER_NOTIFY_OPTIONS` pointing to live stack slices.
    let change_handle = unsafe {
        FindFirstPrinterChangeNotification(
            printer_guard.0,
            PRINTER_CHANGE_ADD_JOB | PRINTER_CHANGE_SET_JOB,
            0,
            Some((&options as *const PRINTER_NOTIFY_OPTIONS).cast::<c_void>()),
        )
    };
    if change_handle.is_invalid() {
        // SAFETY: Capture last Win32 error immediately.
        let err = unsafe { GetLastError() };
        return Err(WatcherError::Win32 {
            api: "FindFirstPrinterChangeNotification",
            message: format!("Win32 error code {}", err.0),
        });
    }
    let change_guard = ChangeNotifyGuard(change_handle);

    // Sweep any jobs already sitting paused/queued at startup.
    if let Ok(initial_ids) = enum_queued_job_ids(printer_guard.0) {
        for job_id in initial_ids {
            if let Ok(Some(dm)) = query_job_devmode(printer_guard.0, printer_name, job_id) {
                if !try_send_devmode(&tx, dm) {
                    return Ok(());
                }
            }
        }
    }

    let wait_handles = [change_guard.0, shutdown_event.as_handle()];
    let shutdown_wait_idx = WAIT_EVENT(WAIT_OBJECT_0.0 + 1);

    while !shutdown_flag.load(Ordering::Acquire) {
        // SAFETY: Both handles (`change_guard.0` and `shutdown_event`) remain open for the loop.
        let wait_res = unsafe { WaitForMultipleObjects(&wait_handles, false, INFINITE) };
        if wait_res == shutdown_wait_idx {
            break;
        }
        if wait_res == WAIT_FAILED {
            // SAFETY: Capture last Win32 error immediately.
            let err = unsafe { GetLastError() };
            return Err(WatcherError::Win32 {
                api: "WaitForMultipleObjects",
                message: format!("Win32 error code {}", err.0),
            });
        }
        if wait_res != WAIT_OBJECT_0 {
            continue;
        }

        let job_ids = collect_notified_job_ids(
            change_guard.0,
            printer_guard.0,
            (&refresh_options as *const PRINTER_NOTIFY_OPTIONS).cast::<c_void>(),
        )?;
        for job_id in job_ids {
            if let Some(dm) = query_job_devmode(printer_guard.0, printer_name, job_id)? {
                if !try_send_devmode(&tx, dm) {
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}
