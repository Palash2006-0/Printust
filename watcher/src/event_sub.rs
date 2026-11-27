//! Stream B: Native Win32 `EvtSubscribe` push subscription + XML bookmark rendering.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows::Win32::System::EventLog::{
    EvtClose, EvtCreateBookmark, EvtRender, EvtRenderBookmark, EvtRenderEventXml, EvtSubscribe,
    EvtSubscribeActionDeliver, EvtSubscribeActionError, EvtSubscribeStartAfterBookmark,
    EvtSubscribeToFutureEvents, EvtUpdateBookmark, EVT_HANDLE, EVT_SUBSCRIBE_NOTIFY_ACTION,
};

use crate::xml_parser::WatcherError;
use crate::StreamEvent;

/// XPath query selecting only PrintService Events `307` (`DocumentPrinted`) and `805` (`RenderJobDiag`).
pub const PRINT_EVENT_XPATH: &str = "*[System[(EventID=307 or EventID=805)]]";

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Owned RAII guard ensuring every created `EVT_HANDLE` is released via `EvtClose`.
struct EvtHandleGuard(EVT_HANDLE);

impl Drop for EvtHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Handle was returned by `EvtCreateBookmark` or `EvtSubscribe` and closed once.
            unsafe {
                let _ = EvtClose(self.0);
            }
        }
    }
}

/// Render an `EVT_HANDLE` (event or bookmark) into a UTF-8 `String` using two-pass `EvtRender`.
fn render_evt_xml(fragment: EVT_HANDLE, flags: u32) -> Result<String, WatcherError> {
    let mut used_bytes: u32 = 0;
    let mut prop_count: u32 = 0;

    // Pass 1: Determine exact UTF-16 byte buffer size.
    // SAFETY: `fragment` is valid and `buffersize = 0` queries `used_bytes`.
    let first_res = unsafe {
        EvtRender(
            EVT_HANDLE::default(),
            fragment,
            flags,
            0,
            None,
            &mut used_bytes,
            &mut prop_count,
        )
    };
    if let Err(err) = first_res {
        if err.code() != windows::core::HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) {
            return Err(WatcherError::Win32 {
                api: "EvtRender(size)",
                message: err.to_string(),
            });
        }
    }

    if used_bytes == 0 {
        return Ok(String::new());
    }

    let u16_cap = (used_bytes as usize).div_ceil(2).max(1);
    let mut buf = vec![0u16; u16_cap];

    // Pass 2: Render into UTF-16 buffer.
    // SAFETY: `buf` provides `(u16_cap * 2)` writable bytes matching `buffersize`.
    unsafe {
        EvtRender(
            EVT_HANDLE::default(),
            fragment,
            flags,
            (buf.len() * 2) as u32,
            Some(buf.as_mut_ptr().cast::<c_void>()),
            &mut used_bytes,
            &mut prop_count,
        )
    }
    .map_err(|err| WatcherError::Win32 {
        api: "EvtRender(data)",
        message: err.to_string(),
    })?;

    let u16_len = ((used_bytes as usize) / 2).min(buf.len());
    let text = String::from_utf16_lossy(&buf[..u16_len])
        .trim_end_matches('\0')
        .to_owned();
    Ok(text)
}

struct EvtCallbackContext {
    tx: SyncSender<StreamEvent>,
    bookmark_handle: Mutex<EvtHandleGuard>,
    shutdown_flag: Arc<AtomicBool>,
    dropped_events: AtomicU64,
}

fn handle_evt_callback(
    action: EVT_SUBSCRIBE_NOTIFY_ACTION,
    ctx: &EvtCallbackContext,
    event: EVT_HANDLE,
) {
    if ctx.shutdown_flag.load(Ordering::Acquire) {
        return;
    }

    if action == EvtSubscribeActionError {
        let _ = ctx.tx.try_send(StreamEvent::Error(WatcherError::Win32 {
            api: "EvtSubscribe(callback)",
            message: format!("Win32 EventLog subscription error notification (handle={})", event.0),
        }));
        return;
    }

    if action != EvtSubscribeActionDeliver || event.is_invalid() {
        return;
    }

    let event_xml = match render_evt_xml(event, EvtRenderEventXml.0) {
        Ok(xml) if !xml.trim().is_empty() => xml,
        Ok(_) => return,
        Err(err) => {
            let _ = ctx.tx.try_send(StreamEvent::Error(err));
            return;
        }
    };

    let bookmark_xml = {
        let Ok(guard) = ctx.bookmark_handle.lock() else {
            return;
        };
        // SAFETY: `guard.0` is an owned bookmark handle from `EvtCreateBookmark`, `event` is delivered by Win32.
        if let Err(err) = unsafe { EvtUpdateBookmark(guard.0, event) } {
            let _ = ctx.tx.try_send(StreamEvent::Error(WatcherError::Win32 {
                api: "EvtUpdateBookmark",
                message: err.to_string(),
            }));
            None
        } else {
            match render_evt_xml(guard.0, EvtRenderBookmark.0) {
                Ok(xml) if !xml.trim().is_empty() => Some(xml),
                Ok(_) => None,
                Err(err) => {
                    let _ = ctx.tx.try_send(StreamEvent::Error(err));
                    None
                }
            }
        }
    };

    // Never block inside a Windows EventLog threadpool callback: `EvtClose` waits for active
    // callbacks to return, so blocking `send()` could deadlock shutdown teardown.
    if let Err(TrySendError::Full(_)) = ctx.tx.try_send(StreamEvent::EventLogXml {
        event_xml,
        bookmark_xml,
    }) {
        let dropped = ctx.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = ctx.tx.try_send(StreamEvent::Error(WatcherError::Win32 {
            api: "EvtSubscribe(callback)",
            message: format!("event stream queue full; dropped {dropped} event(s)"),
        }));
    }
}

/// Minimal Win32 push callback trampoline: renders XML & bookmark and forwards non-blockingly over
/// bounded `mpsc::SyncSender` while preventing any Rust unwind from crossing the FFI boundary.
unsafe extern "system" fn evt_subscribe_callback(
    action: EVT_SUBSCRIBE_NOTIFY_ACTION,
    user_context: *const c_void,
    event: EVT_HANDLE,
) -> u32 {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if user_context.is_null() {
            return;
        }
        // SAFETY: `user_context` was leaked via `Box::into_raw` before `EvtSubscribe` and is freed
        // only *after* `EvtClose(subscription_handle)` completes and waits for callbacks to drain.
        let ctx = unsafe { &*(user_context as *const EvtCallbackContext) };
        handle_evt_callback(action, ctx, event);
    }));
    0
}

/// Active Win32 `EvtSubscribe` subscription whose `Drop` closes the subscription **before**
/// freeing the callback `Box<EvtCallbackContext>`.
pub(crate) struct EventLogSubscription {
    subscription: EvtHandleGuard,
    ctx_ptr: *mut EvtCallbackContext,
}

// SAFETY: Subscription & context pointers are owned cleanly and torn down once via `EvtClose`.
unsafe impl Send for EventLogSubscription {}

impl EventLogSubscription {
    pub(crate) fn start(
        channel_name: &str,
        initial_bookmark_xml: Option<&str>,
        shutdown_flag: Arc<AtomicBool>,
        tx: SyncSender<StreamEvent>,
    ) -> Result<Self, WatcherError> {
        let mut bookmark_warning: Option<String> = None;
        let (bookmark_handle, subscribe_bookmark, flags) = match initial_bookmark_xml {
            Some(xml) if !xml.trim().is_empty() => {
                let wide_bm = to_wide_null(xml);
                // SAFETY: Valid UTF-16 NUL-terminated bookmark XML passed to `EvtCreateBookmark`.
                match unsafe { EvtCreateBookmark(PCWSTR(wide_bm.as_ptr())) } {
                    Ok(bm) => (bm, bm, EvtSubscribeStartAfterBookmark.0),
                    Err(err) => {
                        bookmark_warning = Some(format!(
                            "saved bookmark invalid ({err}); falling back to EvtSubscribeToFutureEvents"
                        ));
                        // SAFETY: `PCWSTR::null()` creates a blank bookmark handle for live tracking.
                        let bm = unsafe { EvtCreateBookmark(PCWSTR::null()) }.map_err(|e| {
                            WatcherError::Win32 {
                                api: "EvtCreateBookmark(empty)",
                                message: e.to_string(),
                            }
                        })?;
                        (bm, EVT_HANDLE::default(), EvtSubscribeToFutureEvents.0)
                    }
                }
            }
            _ => {
                // SAFETY: `PCWSTR::null()` creates a blank bookmark handle for live `EvtUpdateBookmark` calls.
                let bm = unsafe { EvtCreateBookmark(PCWSTR::null()) }.map_err(|err| {
                    WatcherError::Win32 {
                        api: "EvtCreateBookmark(empty)",
                        message: err.to_string(),
                    }
                })?;
                // Cold-start safety: `EvtSubscribeToFutureEvents` requires `EVT_HANDLE::default()`.
                (bm, EVT_HANDLE::default(), EvtSubscribeToFutureEvents.0)
            }
        };

        if let Some(message) = bookmark_warning {
            let _ = tx.try_send(StreamEvent::Error(WatcherError::Win32 {
                api: "EvtCreateBookmark(fallback)",
                message,
            }));
        }

        let ctx_box = Box::new(EvtCallbackContext {
            tx: tx.clone(),
            bookmark_handle: Mutex::new(EvtHandleGuard(bookmark_handle)),
            shutdown_flag,
            dropped_events: AtomicU64::new(0),
        });
        let ctx_ptr = Box::into_raw(ctx_box);

        let wide_channel = to_wide_null(channel_name);
        let wide_query = to_wide_null(PRINT_EVENT_XPATH);

        // SAFETY: Valid channel, XPath query, optional saved bookmark, pinned `ctx_ptr`, and system callback.
        let mut sub_res = unsafe {
            EvtSubscribe(
                EVT_HANDLE::default(),
                HANDLE::default(),
                PCWSTR(wide_channel.as_ptr()),
                PCWSTR(wide_query.as_ptr()),
                subscribe_bookmark,
                Some(ctx_ptr.cast::<c_void>()),
                Some(evt_subscribe_callback),
                flags,
            )
        };

        // If `wevtutil cl` cleared the channel or the 1 MB `.evtx` log wrapped past the saved
        // bookmark RecordId, warn non-fatally and fall back to live `EvtSubscribeToFutureEvents`.
        if let (Err(err), true) = (&sub_res, flags == EvtSubscribeStartAfterBookmark.0) {
            let _ = tx.try_send(StreamEvent::Error(WatcherError::Win32 {
                api: "EvtSubscribe(bookmark_fallback)",
                message: format!(
                    "stale bookmark resume failed ({err}); falling back to EvtSubscribeToFutureEvents"
                ),
            }));

            // SAFETY: No callbacks are registered because `EvtSubscribe` returned `Err`.
            let fresh_bm = match unsafe { EvtCreateBookmark(PCWSTR::null()) } {
                Ok(bm) => bm,
                Err(e) => {
                    // SAFETY: Reclaim `ctx_ptr` cleanly on setup error.
                    unsafe {
                        drop(Box::from_raw(ctx_ptr));
                    }
                    return Err(WatcherError::Win32 {
                        api: "EvtCreateBookmark(fallback_empty)",
                        message: e.to_string(),
                    });
                }
            };

            // SAFETY: `ctx_ptr` remains valid until reclaimed on error or `Drop`.
            let ctx_ref = unsafe { &*ctx_ptr };
            if let Ok(mut guard) = ctx_ref.bookmark_handle.lock() {
                *guard = EvtHandleGuard(fresh_bm);
            } else {
                // SAFETY: Close `fresh_bm` if mutex somehow poisoned.
                unsafe {
                    let _ = EvtClose(fresh_bm);
                }
            }

            // SAFETY: Retrying subscription in live future-only mode with `EVT_HANDLE::default()` bookmark.
            sub_res = unsafe {
                EvtSubscribe(
                    EVT_HANDLE::default(),
                    HANDLE::default(),
                    PCWSTR(wide_channel.as_ptr()),
                    PCWSTR(wide_query.as_ptr()),
                    EVT_HANDLE::default(),
                    Some(ctx_ptr.cast::<c_void>()),
                    Some(evt_subscribe_callback),
                    EvtSubscribeToFutureEvents.0,
                )
            };
        }

        let sub_handle = match sub_res {
            Ok(h) => h,
            Err(err) => {
                // SAFETY: Subscription failed to start; reclaim `ctx_ptr` immediately with no callbacks possible.
                unsafe {
                    drop(Box::from_raw(ctx_ptr));
                }
                return Err(WatcherError::Win32 {
                    api: "EvtSubscribe",
                    message: err.to_string(),
                });
            }
        };

        Ok(Self {
            subscription: EvtHandleGuard(sub_handle),
            ctx_ptr,
        })
    }
}

impl Drop for EventLogSubscription {
    fn drop(&mut self) {
        // Critical teardown order: close subscription FIRST (which waits for active callbacks
        // in `wevtapi.dll`), invalidate handle so `EvtHandleGuard::drop` doesn't close twice,
        // and ONLY THEN reclaim `ctx_ptr`.
        if !self.subscription.0.is_invalid() {
            // SAFETY: Valid subscription handle from `EvtSubscribe`.
            unsafe {
                let _ = EvtClose(self.subscription.0);
            }
            self.subscription.0 = EVT_HANDLE::default();
        }
        if !self.ctx_ptr.is_null() {
            // SAFETY: No callbacks remain running once `EvtClose(subscription)` returns.
            unsafe {
                drop(Box::from_raw(self.ctx_ptr));
            }
            self.ctx_ptr = std::ptr::null_mut();
        }
    }
}
