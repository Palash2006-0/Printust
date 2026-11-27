//! Native Windows spooler watcher (`printust_watcher`) combining `JOB_INFO_2W` + `EvtSubscribe`.

pub mod correlator;
pub mod event_sub;
pub mod spooler_hook;
pub mod xml_parser;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use correlator::{CachedDevMode, CorrelatedJob, JobCorrelator};
use event_sub::EventLogSubscription;
use serde::{Deserialize, Serialize};
use spooler_hook::{run_spooler_hook, ShutdownEvent};
use xml_parser::{parse_spooler_event_xml, WatcherError};

/// Default Windows Event Log channel monitored for Events `307` and `805`.
pub const DEFAULT_EVENT_CHANNEL: &str = "Microsoft-Windows-PrintService/Operational";

/// Configuration for starting [`WatcherService`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatcherConfig {
    pub printer_name: String,
    pub channel_name: String,
    pub channel_capacity: usize,
}

impl WatcherConfig {
    /// Construct a default watcher configuration for a given printer queue name (e.g., `ShopTest_PCL6`).
    pub fn for_printer(printer_name: impl Into<String>) -> Self {
        Self {
            printer_name: printer_name.into(),
            channel_name: DEFAULT_EVENT_CHANNEL.to_owned(),
            channel_capacity: 256,
        }
    }
}

/// Messages sent from Win32 producer hooks into the correlator consumer thread.
#[derive(Debug)]
pub(crate) enum StreamEvent {
    DevModeCaptured(CachedDevMode),
    EventLogXml {
        event_xml: String,
        bookmark_xml: Option<String>,
    },
    Error(WatcherError),
}

/// Output stream produced by the correlator thread for persistence/UI emission.
#[derive(Debug)]
pub enum WatcherOutput {
    JobReady(CorrelatedJob),
    WarningOrError(WatcherError),
}

/// Lifecycle handle allowing graceful shutdown of both Stream A (`GetJobW`), Stream B (`EvtSubscribe`),
/// and the correlator consumer thread.
pub struct WatcherHandle {
    shutdown_flag: Arc<AtomicBool>,
    shutdown_event: Arc<ShutdownEvent>,
    subscription: Option<EventLogSubscription>,
    spooler_thread: Option<JoinHandle<()>>,
    consumer_thread: Option<JoinHandle<()>>,
}

impl WatcherHandle {
    /// Signal shutdown and wait cleanly for Win32 subscriptions and threads to exit.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.shutdown_flag.store(true, Ordering::Release);
        let _ = self.shutdown_event.signal();

        // Drop `EvtSubscribe` subscription first so callbacks stop arriving.
        drop(self.subscription.take());

        if let Some(handle) = self.spooler_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.consumer_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for WatcherHandle {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// Entrypoint spawning Stream A (`spooler_hook`), Stream B (`event_sub`), and the correlator loop.
pub struct WatcherService;

impl WatcherService {
    /// Start native spooler + Event Log monitoring and return a lifecycle handle alongside a
    /// [`Receiver<WatcherOutput>`] carrying reconciled [`CorrelatedJob`] records.
    pub fn start(
        config: WatcherConfig,
        initial_bookmark_xml: Option<String>,
    ) -> Result<(WatcherHandle, Receiver<WatcherOutput>), WatcherError> {
        let cap = config.channel_capacity.max(16);
        let (raw_tx, raw_rx) = mpsc::sync_channel::<StreamEvent>(cap);
        let (out_tx, out_rx) = mpsc::sync_channel::<WatcherOutput>(cap);

        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown_event = Arc::new(ShutdownEvent::new_manual_reset()?);

        // Start Stream B (`EvtSubscribe`): fails loudly if channel/query cannot be opened.
        let subscription = EventLogSubscription::start(
            &config.channel_name,
            initial_bookmark_xml.as_deref(),
            Arc::clone(&shutdown_flag),
            raw_tx.clone(),
        )?;

        // Start Stream A (`OpenPrinterW` + `FindFirstPrinterChangeNotification` + `GetJobW`).
        let printer_name = config.printer_name.clone();
        let spooler_shutdown_flag = Arc::clone(&shutdown_flag);
        let spooler_shutdown_evt = Arc::clone(&shutdown_event);
        let spooler_tx = raw_tx.clone();
        let spooler_thread = thread::Builder::new()
            .name("printust-spooler-hook".to_owned())
            .spawn(move || {
                if let Err(err) = run_spooler_hook(
                    &printer_name,
                    spooler_shutdown_evt,
                    spooler_shutdown_flag,
                    spooler_tx.clone(),
                ) {
                    let _ = spooler_tx.try_send(StreamEvent::Error(err));
                }
            })
            .map_err(|err| WatcherError::Win32 {
                api: "thread::spawn(spooler_hook)",
                message: err.to_string(),
            })?;

        drop(raw_tx);

        // Start consumer thread running `JobCorrelator` and periodic 100ms timeout flushes.
        let consumer_shutdown = Arc::clone(&shutdown_flag);
        let consumer_thread = thread::Builder::new()
            .name("printust-correlator".to_owned())
            .spawn(move || {
                let mut correlator = JobCorrelator::new();
                let tick = Duration::from_millis(100);

                loop {
                    if consumer_shutdown.load(Ordering::Acquire) {
                        return;
                    }
                    match raw_rx.recv_timeout(tick) {
                        Ok(StreamEvent::DevModeCaptured(dm)) => {
                            correlator.record_devmode(dm);
                        }
                        Ok(StreamEvent::EventLogXml {
                            event_xml,
                            bookmark_xml,
                        }) => match parse_spooler_event_xml(&event_xml) {
                            Ok(parsed) => {
                                for ready in correlator.process_spooler_event(parsed, bookmark_xml)
                                {
                                    if !send_output_cancellable(
                                        &out_tx,
                                        WatcherOutput::JobReady(ready),
                                        &consumer_shutdown,
                                    ) {
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                if !send_output_cancellable(
                                    &out_tx,
                                    WatcherOutput::WarningOrError(err),
                                    &consumer_shutdown,
                                ) {
                                    return;
                                }
                            }
                        },
                        Ok(StreamEvent::Error(err)) => {
                            if !send_output_cancellable(
                                &out_tx,
                                WatcherOutput::WarningOrError(err),
                                &consumer_shutdown,
                            ) {
                                return;
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            for flushed in correlator.drain_expired_at(Instant::now()) {
                                if !send_output_cancellable(
                                    &out_tx,
                                    WatcherOutput::JobReady(flushed),
                                    &consumer_shutdown,
                                ) {
                                    return;
                                }
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => {
                            let far_future = Instant::now() + Duration::from_secs(10);
                            for flushed in correlator.drain_expired_at(far_future) {
                                if !send_output_cancellable(
                                    &out_tx,
                                    WatcherOutput::JobReady(flushed),
                                    &consumer_shutdown,
                                ) {
                                    return;
                                }
                            }
                            return;
                        }
                    }
                }
            })
            .map_err(|err| WatcherError::Win32 {
                api: "thread::spawn(correlator)",
                message: err.to_string(),
            })?;

        let handle = WatcherHandle {
            shutdown_flag,
            shutdown_event,
            subscription: Some(subscription),
            spooler_thread: Some(spooler_thread),
            consumer_thread: Some(consumer_thread),
        };

        Ok((handle, out_rx))
    }
}

fn send_output_cancellable(
    out_tx: &SyncSender<WatcherOutput>,
    msg: WatcherOutput,
    shutdown: &AtomicBool,
) -> bool {
    let mut pending = msg;
    while !shutdown.load(Ordering::Acquire) {
        match out_tx.try_send(pending) {
            Ok(()) => return true,
            Err(TrySendError::Disconnected(_)) => return false,
            Err(TrySendError::Full(returned)) => {
                pending = returned;
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    false
}
