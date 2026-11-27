use std::mem::{align_of, size_of};
use std::time::{Duration, Instant};

use printust_core::{ColorMode, DuplexMode, IngestOutcome, LedgerDb, SessionEngine};
use printust_watcher::correlator::{
    CachedDevMode, JobCorrelator, DEVMODE_CACHE_TTL, LATE_805_WAIT_TTL, PENDING_805_TTL,
};
use printust_watcher::spooler_hook::extract_devmode_from_job_info2_buf;
use printust_watcher::xml_parser::{
    parse_spooler_event_xml, Event307Data, Event805Data, ParsedSpoolerEvent, WatcherError,
};
use windows::core::PWSTR;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DMCOLOR_COLOR, DMDUP_HORIZONTAL, DM_COLOR, DM_COPIES, DM_DUPLEX, DM_PAPERSIZE,
};
use windows::Win32::Graphics::Printing::JOB_INFO_2W;

const SPIKE_307_XML: &str =
    include_str!("../../spike/raw_xml_samples/event_307_rec_318_job_10.xml");
const SPIKE_805_XML: &str =
    include_str!("../../spike/raw_xml_samples/event_805_rec_317_job_10.xml");

fn build_event_307_xml(
    rec_id: i64,
    job_id: u32,
    doc: &str,
    printer: &str,
    pages_printed: u32,
    ts: &str,
) -> String {
    format!(
        "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
           <System>\
             <EventID>307</EventID>\
             <TimeCreated SystemTime='{ts}'/>\
             <EventRecordID>{rec_id}</EventRecordID>\
           </System>\
           <UserData>\
             <DocumentPrinted xmlns='http://manifests.microsoft.com/win/2005/08/windows/printing/spooler/core/events'>\
               <Param1>{job_id}</Param1>\
               <Param2>{doc}</Param2>\
               <Param3>SHOP_USER</Param3>\
               <Param4>\\\\SHOP_PC</Param4>\
               <Param5>{printer}</Param5>\
               <Param6>NUL</Param6>\
               <Param7>4096</Param7>\
               <Param8>{pages_printed}</Param8>\
             </DocumentPrinted>\
           </UserData>\
         </Event>"
    )
}

fn build_event_805_xml(rec_id: i64, job_id: u32, copies: u32, color: u32, ts: &str) -> String {
    format!(
        "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'>\
           <System>\
             <EventID>805</EventID>\
             <TimeCreated SystemTime='{ts}'/>\
             <EventRecordID>{rec_id}</EventRecordID>\
           </System>\
           <UserData>\
             <RenderJobDiag xmlns='http://manifests.microsoft.com/win/2005/08/windows/printing/spooler/core/events'>\
               <JobId>{job_id}</JobId>\
               <GdiJobSize>123456</GdiJobSize>\
               <Color>{color}</Color>\
               <Copies>{copies}</Copies>\
             </RenderJobDiag>\
           </UserData>\
         </Event>"
    )
}

#[test]
fn parses_real_captured_spike_307_and_805_xml_samples() {
    let ev805 = parse_spooler_event_xml(SPIKE_805_XML).unwrap();
    match ev805 {
        ParsedSpoolerEvent::RenderDiag805(d) => {
            assert_eq!(d.record_id, 317);
            assert_eq!(d.job_id, 10);
            assert_eq!(d.copies, 3);
            assert_eq!(d.color, ColorMode::Monochrome);
            assert_eq!(d.gdi_job_size, 9_240_576);
            assert_eq!(d.timestamp, "2026-11-25T13:11:39.2101117Z");
        }
        other => panic!("expected RenderDiag805, got {other:?}"),
    }

    let ev307 = parse_spooler_event_xml(SPIKE_307_XML).unwrap();
    match ev307 {
        ParsedSpoolerEvent::DocumentPrinted307(d) => {
            assert_eq!(d.record_id, 318);
            assert_eq!(d.job_id, 10);
            assert_eq!(d.doc_name, "Print Document");
            assert_eq!(d.printer_name, "ShopTest_PCL6");
            assert_eq!(d.port_name, "NUL");
            assert_eq!(d.size_bytes, 9_240_576);
            assert_eq!(d.pages_printed, 9);
            assert_eq!(d.timestamp, "2026-11-25T13:11:39.2103292Z");
        }
        other => panic!("expected DocumentPrinted307, got {other:?}"),
    }
}

#[test]
fn parses_named_307_xml_aliases_and_rejects_malformed_or_unsupported_xml() {
    let named_307 = r#"
        <Event>
          <System>
            <EventID>307</EventID>
            <EventRecordID>500</EventRecordID>
            <TimeCreated SystemTime="2026-11-27T15:00:00Z"/>
          </System>
          <UserData>
            <DocumentPrinted>
              <JobId>22</JobId>
              <DocumentName>thesis.pdf</DocumentName>
              <PrinterName>ShopTest_PCL6</PrinterName>
              <PagesPrinted>7</PagesPrinted>
              <SizeBytes>88000</SizeBytes>
            </DocumentPrinted>
          </UserData>
        </Event>
    "#;
    match parse_spooler_event_xml(named_307).unwrap() {
        ParsedSpoolerEvent::DocumentPrinted307(d) => {
            assert_eq!(d.job_id, 22);
            assert_eq!(d.doc_name, "thesis.pdf");
            assert_eq!(d.pages_printed, 7);
            assert_eq!(d.size_bytes, 88_000);
        }
        other => panic!("unexpected event variant: {other:?}"),
    }

    assert!(matches!(
        parse_spooler_event_xml("<Event><System><EventID>307").unwrap_err(),
        WatcherError::XmlParse(_)
    ));
    assert!(matches!(
        parse_spooler_event_xml("<Event><UserData/></Event>").unwrap_err(),
        WatcherError::XmlParse(_)
    ));

    let unsupported = "<Event><System><EventID>305</EventID><EventRecordID>1</EventRecordID><TimeCreated SystemTime='2026-11-27T00:00:00Z'/></System></Event>";
    assert!(matches!(
        parse_spooler_event_xml(unsupported).unwrap_err(),
        WatcherError::UnsupportedEventId(305)
    ));
}

#[test]
fn correlates_normal_devmode_805_307_unmasks_doc_and_evicts_caches() {
    let t0 = Instant::now();
    let mut correlator = JobCorrelator::new();

    correlator.record_devmode_at(
        CachedDevMode {
            printer_name: "ShopTest_PCL6".to_owned(),
            job_id: 11,
            doc_name: "assignment_real.pdf".to_owned(),
            total_pages: 3,
            copies: Some(2),
            duplex_mode: DuplexMode::DuplexLongEdge,
            color_mode: ColorMode::Monochrome,
            paper_size: Some(9),
        },
        t0,
    );
    assert_eq!(correlator.devmode_cache_len(), 1);

    let ev805 = parse_spooler_event_xml(&build_event_805_xml(
        401,
        11,
        2,
        1,
        "2026-11-27T15:01:00Z",
    ))
    .unwrap();
    assert!(correlator
        .process_spooler_event_at(ev805, None, t0 + Duration::from_millis(5))
        .is_empty());
    assert_eq!(correlator.pending_805_len(), 1);

    let ev307 = parse_spooler_event_xml(&build_event_307_xml(
        402,
        11,
        "Print Document",
        "ShopTest_PCL6",
        6,
        "2026-11-27T15:01:01Z",
    ))
    .unwrap();
    let ready = correlator.process_spooler_event_at(
        ev307,
        Some("<BookmarkList>bm-402</BookmarkList>".to_owned()),
        t0 + Duration::from_millis(10),
    );
    assert_eq!(ready.len(), 1);
    let item = &ready[0];
    assert!(!item.used_late_805);
    assert_eq!(item.job.doc_name, "assignment_real.pdf");
    assert_eq!(item.job.raw_pages, 3);
    assert_eq!(item.job.raw_copies, 2);
    assert_eq!(item.job.effective_pages, 6);
    assert_eq!(item.job.duplex_mode, DuplexMode::DuplexLongEdge);
    assert_eq!(item.job.driver_anomaly_flag, None);
    // Both caches must be immediately evicted once 307 finishes correlation
    assert_eq!(correlator.devmode_cache_len(), 0);
    assert_eq!(correlator.pending_805_len(), 0);
}

#[test]
fn late_805_within_record_gap_two_reconciles_while_gap_over_two_flushes_without_805() {
    let t0 = Instant::now();
    let mut correlator = JobCorrelator::new();

    // Event 307 arrives first (RecId 500, JobId 12)
    let ev307 = parse_spooler_event_xml(&build_event_307_xml(
        500,
        12,
        "photo.pdf",
        "ShopTest_PCL6",
        2,
        "2026-11-27T15:02:00Z",
    ))
    .unwrap();
    assert!(correlator.process_spooler_event_at(ev307, None, t0).is_empty());
    assert_eq!(correlator.pending_307_len(), 1);

    // Late 805 arrives 20ms later with RecId 502 (gap = 2 <= LATE_805_MAX_RECORD_GAP) and Color=2
    let late805 = parse_spooler_event_xml(&build_event_805_xml(
        502,
        12,
        1,
        2,
        "2026-11-27T15:02:00Z",
    ))
    .unwrap();
    let ready = correlator.process_spooler_event_at(
        late805,
        Some("<BookmarkList>bm-502</BookmarkList>".to_owned()),
        t0 + Duration::from_millis(20),
    );
    assert_eq!(ready.len(), 1);
    assert!(ready[0].used_late_805);
    assert_eq!(ready[0].job.color_mode, ColorMode::Color);
    assert_eq!(ready[0].job.duplex_mode, DuplexMode::Unknown);
    assert_eq!(
        ready[0].job.driver_anomaly_flag.as_deref(),
        Some("DEVMODE_RACE_MISSED")
    );
    assert_eq!(correlator.pending_307_len(), 0);

    // Case 2: Event 307 arrives first (RecId 600, JobId 13), followed by 805 with gap=3 (603) -> ignored,
    // then flushed after LATE_805_WAIT_TTL with EVENT_805_MISSING (provided DevMode exists)
    let t1 = t0 + Duration::from_secs(1);
    correlator.record_devmode_at(
        CachedDevMode {
            printer_name: "ShopTest_PCL6".to_owned(),
            job_id: 13,
            doc_name: "notes.pdf".to_owned(),
            total_pages: 1,
            copies: Some(1),
            duplex_mode: DuplexMode::Simplex,
            color_mode: ColorMode::Monochrome,
            paper_size: None,
        },
        t1,
    );
    let ev307_gap = parse_spooler_event_xml(&build_event_307_xml(
        600,
        13,
        "notes.pdf",
        "ShopTest_PCL6",
        1,
        "2026-11-27T15:03:00Z",
    ))
    .unwrap();
    assert!(correlator.process_spooler_event_at(ev307_gap, None, t1).is_empty());

    let too_far_805 = parse_spooler_event_xml(&build_event_805_xml(
        603,
        13,
        2,
        2,
        "2026-11-27T15:03:01Z",
    ))
    .unwrap();
    assert!(correlator
        .process_spooler_event_at(too_far_805, None, t1 + Duration::from_millis(50))
        .is_empty());
    assert_eq!(correlator.pending_307_len(), 1);

    let flushed = correlator.drain_expired_at(t1 + LATE_805_WAIT_TTL + Duration::from_millis(1));
    assert_eq!(flushed.len(), 1);
    assert_eq!(
        flushed[0].job.driver_anomaly_flag.as_deref(),
        Some("EVENT_805_MISSING")
    );
}

#[test]
fn prunes_stale_805_after_15s_and_stale_devmode_after_10_minutes() {
    let t0 = Instant::now();
    let mut correlator = JobCorrelator::new();

    correlator.record_devmode_at(
        CachedDevMode {
            printer_name: "ShopTest_PCL6".to_owned(),
            job_id: 99,
            doc_name: "old_stale.pdf".to_owned(),
            total_pages: 4,
            copies: Some(1),
            duplex_mode: DuplexMode::DuplexLongEdge,
            color_mode: ColorMode::Color,
            paper_size: None,
        },
        t0,
    );
    let old_805 = ParsedSpoolerEvent::RenderDiag805(Event805Data {
        record_id: 100,
        timestamp: "2026-11-27T14:00:00Z".to_owned(),
        job_id: 99,
        gdi_job_size: 1000,
        copies: 5,
        color: ColorMode::Color,
        icm_method: None,
        x_res: None,
        y_res: None,
        quality: None,
        tt_option: None,
    });
    assert!(correlator.process_spooler_event_at(old_805, None, t0).is_empty());
    assert_eq!(correlator.pending_805_len(), 1);
    assert_eq!(correlator.devmode_cache_len(), 1);

    // At 16 seconds: 805 (> 15s) is pruned, while DevMode (< 600s) survives
    let t_16s = t0 + PENDING_805_TTL + Duration::from_secs(1);
    correlator.drain_expired_at(t_16s);
    assert_eq!(correlator.pending_805_len(), 0);
    assert_eq!(correlator.devmode_cache_len(), 1);

    // After > 10 minutes: stale DevMode is also pruned so a recycled JobId=99 cannot inherit it
    let t_10m = t0 + DEVMODE_CACHE_TTL + Duration::from_secs(1);
    let reused_307 = ParsedSpoolerEvent::DocumentPrinted307(Event307Data {
        record_id: 200,
        timestamp: "2026-11-27T14:11:00Z".to_owned(),
        job_id: 99,
        doc_name: "new_recycled.pdf".to_owned(),
        user_name: "SHOP_USER".to_owned(),
        machine_name: "SHOP_PC".to_owned(),
        printer_name: "ShopTest_PCL6".to_owned(),
        port_name: "NUL".to_owned(),
        size_bytes: 2048,
        pages_printed: 1,
    });
    assert!(correlator.process_spooler_event_at(reused_307, None, t_10m).is_empty());
    assert_eq!(correlator.devmode_cache_len(), 0);

    let flushed = correlator.drain_expired_at(t_10m + LATE_805_WAIT_TTL + Duration::from_millis(1));
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0].job.duplex_mode, DuplexMode::Unknown);
    assert_eq!(
        flushed[0].job.driver_anomaly_flag.as_deref(),
        Some("DEVMODE_RACE_MISSED")
    );
}

#[test]
fn chrome_unrolled_multi_copy_and_devmode_race_persist_atomically_into_core_ledger() {
    let t0 = Instant::now();
    let mut correlator = JobCorrelator::new();

    // Replay real Chrome spike capture (JobId=10: 3p x 3c -> Event 805 Copies=3, Event 307 PagesPrinted=9)
    // WITHOUT DevMode (simulating an ultra-fast unpaused GetJobW miss).
    let ev805 = parse_spooler_event_xml(SPIKE_805_XML).unwrap();
    let ev307 = parse_spooler_event_xml(SPIKE_307_XML).unwrap();
    assert!(correlator.process_spooler_event_at(ev805, None, t0).is_empty());

    let bookmark = "<BookmarkList><Bookmark Channel='Microsoft-Windows-PrintService/Operational' RecordId='318'/></BookmarkList>";
    let ready = correlator.process_spooler_event_at(
        ev307,
        Some(bookmark.to_owned()),
        t0 + Duration::from_millis(5),
    );
    assert_eq!(ready.len(), 1);
    let item = &ready[0];
    assert_eq!(item.job.windows_job_id, 10);
    assert_eq!(item.job.raw_copies, 3);
    assert_eq!(item.job.effective_pages, 9);
    assert_eq!(item.job.duplex_mode, DuplexMode::Unknown);
    assert_eq!(
        item.job.driver_anomaly_flag.as_deref(),
        Some("DEVMODE_RACE_MISSED")
    );

    // Persist into LedgerDb + SessionEngine and confirm NO double-multiplication (9 * 200 = 1800 paise)
    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);
    let channel = "Microsoft-Windows-PrintService/Operational";

    let outcome = engine
        .ingest_print_job_with_bookmark(item.job.clone(), channel, bookmark)
        .unwrap();
    match outcome {
        IngestOutcome::Applied(summary) => {
            assert_eq!(summary.bw_pages, 9);
            assert_eq!(summary.total_amount_paise, 1800);
            assert!(summary.has_anomaly);
        }
        IngestOutcome::DuplicateIgnored => panic!("expected first ingest to apply"),
    }

    // Replaying exact same XML event after restart is idempotently ignored
    let dup = engine
        .ingest_print_job_with_bookmark(item.job.clone(), channel, bookmark)
        .unwrap();
    assert_eq!(dup, IngestOutcome::DuplicateIgnored);
}

#[test]
fn unsafe_job_info_2_memory_guards_validate_pointers_alignment_bounds_and_dmsize() {
    // Allocate a u64-backed buffer large enough for JOB_INFO_2W + UTF-16 doc title + DEVMODEW.
    let job_size = size_of::<JOB_INFO_2W>();
    let dm_offset = (job_size + align_of::<DEVMODEW>() - 1) & !(align_of::<DEVMODEW>() - 1);
    let dm_size = size_of::<DEVMODEW>();
    let doc_offset = (dm_offset + dm_size + 1) & !1;
    let doc_u16: Vec<u16> = "live_queue_doc.pdf\0".encode_utf16().collect();
    let total_bytes = doc_offset + doc_u16.len() * 2;

    let mut backing = vec![0u64; total_bytes.div_ceil(8)];
    // SAFETY: `backing` owns `backing.len() * 8 >= total_bytes` contiguous aligned bytes.
    let buf: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), total_bytes) };
    let base_addr = buf.as_ptr() as usize;

    let mut dm = DEVMODEW {
        dmSize: dm_size as u16,
        dmFields: DM_DUPLEX | DM_COPIES | DM_COLOR | DM_PAPERSIZE,
        dmDuplex: DMDUP_HORIZONTAL,
        dmColor: DMCOLOR_COLOR,
        ..DEVMODEW::default()
    };
    dm.Anonymous1.Anonymous1.dmCopies = 3;
    dm.Anonymous1.Anonymous1.dmPaperSize = 9; // A4

    let mut job_info = JOB_INFO_2W {
        JobId: 44,
        TotalPages: 5,
        pDevMode: (base_addr + dm_offset) as *mut DEVMODEW,
        pDocument: PWSTR((base_addr + doc_offset) as *mut u16),
        ..JOB_INFO_2W::default()
    };

    // SAFETY: Offsets and alignments were computed strictly within `buf`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&job_info as *const JOB_INFO_2W).cast::<u8>(),
            buf.as_mut_ptr(),
            job_size,
        );
        std::ptr::copy_nonoverlapping(
            (&dm as *const DEVMODEW).cast::<u8>(),
            buf.as_mut_ptr().add(dm_offset),
            dm_size,
        );
        std::ptr::copy_nonoverlapping(
            doc_u16.as_ptr().cast::<u8>(),
            buf.as_mut_ptr().add(doc_offset),
            doc_u16.len() * 2,
        );
    }

    let extracted = extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).unwrap();
    assert_eq!(extracted.job_id, 44);
    assert_eq!(extracted.total_pages, 5);
    assert_eq!(extracted.doc_name, "live_queue_doc.pdf");
    assert_eq!(extracted.copies, Some(3));
    assert_eq!(extracted.duplex_mode, DuplexMode::DuplexShortEdge);
    assert_eq!(extracted.color_mode, ColorMode::Color);
    assert_eq!(extracted.paper_size, Some(9));

    // Guard check A: Undersized dmSize must be rejected safely
    let mut bad_dm = dm;
    bad_dm.dmSize = (dm_size - 4) as u16;
    // SAFETY: Writing within valid buffer slice at `dm_offset`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&bad_dm as *const DEVMODEW).cast::<u8>(),
            buf.as_mut_ptr().add(dm_offset),
            dm_size,
        );
    }
    assert!(extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).is_none());

    // Guard check B: Out-of-bounds pDevMode pointer must be rejected without dereferencing
    job_info.pDevMode = (base_addr + total_bytes) as *mut DEVMODEW;
    // SAFETY: Overwriting `JOB_INFO_2W` header within buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&job_info as *const JOB_INFO_2W).cast::<u8>(),
            buf.as_mut_ptr(),
            job_size,
        );
    }
    assert!(extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).is_none());

    // Guard check C: Unaligned pDevMode pointer must be rejected safely
    job_info.pDevMode = (base_addr + dm_offset + 1) as *mut DEVMODEW;
    // SAFETY: Overwriting `JOB_INFO_2W` header within buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&job_info as *const JOB_INFO_2W).cast::<u8>(),
            buf.as_mut_ptr(),
            job_size,
        );
    }
    assert!(extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).is_none());

    // Guard check D: Null pDevMode gracefully yields Unknown duplex/color while keeping doc_name
    job_info.pDevMode = std::ptr::null_mut();
    // SAFETY: Overwriting `JOB_INFO_2W` header within buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&job_info as *const JOB_INFO_2W).cast::<u8>(),
            buf.as_mut_ptr(),
            job_size,
        );
    }
    let null_dm = extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).unwrap();
    assert_eq!(null_dm.doc_name, "live_queue_doc.pdf");
    assert_eq!(null_dm.duplex_mode, DuplexMode::Unknown);
    assert_eq!(null_dm.color_mode, ColorMode::Unknown);
}

#[test]
fn unset_dm_duplex_field_bit_falls_back_to_unknown_without_trusting_garbage() {
    let job_size = size_of::<JOB_INFO_2W>();
    let dm_offset = (job_size + align_of::<DEVMODEW>() - 1) & !(align_of::<DEVMODEW>() - 1);
    let dm_size = size_of::<DEVMODEW>();
    let total_bytes = dm_offset + dm_size;

    let mut backing = vec![0u64; total_bytes.div_ceil(8)];
    // SAFETY: `backing` owns `backing.len() * 8 >= total_bytes` aligned bytes.
    let buf: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(backing.as_mut_ptr().cast::<u8>(), total_bytes) };
    let base_addr = buf.as_ptr() as usize;

    // Garbage/stale dmDuplex=2 with DM_DUPLEX bit UNSET in dmFields -> must yield DuplexMode::Unknown
    let dm = DEVMODEW {
        dmSize: dm_size as u16,
        dmFields: DM_COPIES,
        dmDuplex: DMDUP_HORIZONTAL,
        ..DEVMODEW::default()
    };
    let job_info = JOB_INFO_2W {
        JobId: 55,
        TotalPages: 2,
        pDevMode: (base_addr + dm_offset) as *mut DEVMODEW,
        ..JOB_INFO_2W::default()
    };

    // SAFETY: Offsets lie strictly within `buf`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&job_info as *const JOB_INFO_2W).cast::<u8>(),
            buf.as_mut_ptr(),
            job_size,
        );
        std::ptr::copy_nonoverlapping(
            (&dm as *const DEVMODEW).cast::<u8>(),
            buf.as_mut_ptr().add(dm_offset),
            dm_size,
        );
    }

    let extracted = extract_devmode_from_job_info2_buf("ShopTest_PCL6", buf).unwrap();
    assert_eq!(extracted.duplex_mode, DuplexMode::Unknown);
    assert_eq!(extracted.color_mode, ColorMode::Unknown);
}

#[test]
fn multi_copy_odd_duplex_with_devmode_total_pages_bills_per_copy_end_to_end() {
    let t0 = Instant::now();
    let mut correlator = JobCorrelator::new();

    correlator.record_devmode_at(
        CachedDevMode {
            printer_name: "ShopTest_PCL6".to_owned(),
            job_id: 77,
            doc_name: "brochure.pdf".to_owned(),
            total_pages: 3,
            copies: Some(2),
            duplex_mode: DuplexMode::DuplexShortEdge,
            color_mode: ColorMode::Monochrome,
            paper_size: Some(9),
        },
        t0,
    );

    let ev805 = parse_spooler_event_xml(&build_event_805_xml(
        701,
        77,
        2,
        1,
        "2026-11-27T16:00:00Z",
    ))
    .unwrap();
    assert!(correlator.process_spooler_event_at(ev805, None, t0).is_empty());

    let ev307 = parse_spooler_event_xml(&build_event_307_xml(
        702,
        77,
        "Print Document",
        "ShopTest_PCL6",
        6,
        "2026-11-27T16:00:01Z",
    ))
    .unwrap();
    let ready = correlator.process_spooler_event_at(
        ev307,
        Some("<BookmarkList>bm-702</BookmarkList>".to_owned()),
        t0 + Duration::from_millis(5),
    );
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].job.raw_pages, 3);
    assert_eq!(ready[0].job.raw_copies, 2);
    assert_eq!(ready[0].job.effective_pages, 6);

    let db = LedgerDb::open_in_memory().unwrap();
    let mut engine = SessionEngine::new(db);
    let outcome = engine
        .ingest_print_job_with_bookmark(
            ready[0].job.clone(),
            "Microsoft-Windows-PrintService/Operational",
            ready[0].bookmark_xml.as_deref().unwrap(),
        )
        .unwrap();
    match outcome {
        IngestOutcome::Applied(summary) => {
            // 3 pages x 2 copies B&W duplex = (300 + 200) * 2 = 1000 paise
            assert_eq!(summary.total_amount_paise, 1000);
            assert!(!summary.has_anomaly);
        }
        IngestOutcome::DuplicateIgnored => panic!("expected applied outcome"),
    }
}
