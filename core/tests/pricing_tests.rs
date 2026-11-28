use std::str::FromStr;

use printust_core::models::{
    ColorMode, DuplexMode, IncomingPrintJob, LedgerError, PricingRules, SessionStatus,
    SessionSummary,
};
use printust_core::pricing::{calculate_job_cost_for_modes, calculate_job_cost_paise};

#[test]
fn zero_pages_or_zero_copies_cost_is_zero() {
    let rules = PricingRules::default();
    assert_eq!(calculate_job_cost_paise(0, 1, false, false, &rules), 0);
    assert_eq!(calculate_job_cost_paise(5, 0, true, true, &rules), 0);
    assert_eq!(calculate_job_cost_paise(0, 0, false, true, &rules), 0);
}

#[test]
fn bw_simplex_charges_per_side() {
    let rules = PricingRules::default();
    // 1 page x 1 copy @ Rs. 2.00 = 200 paise
    assert_eq!(calculate_job_cost_paise(1, 1, false, false, &rules), 200);
    // 2 pages x 3 copies = 6 sides @ Rs. 2.00 = 1200 paise
    assert_eq!(calculate_job_cost_paise(2, 3, false, false, &rules), 1200);
}

#[test]
fn bw_even_duplex_charges_per_two_sided_sheet() {
    let rules = PricingRules::default();
    // 4 sides = 2 full duplex sheets @ Rs. 3.00 = 600 paise
    assert_eq!(calculate_job_cost_paise(4, 1, false, true, &rules), 600);
    // 2 pages x 3 copies = 6 sides = 3 duplex sheets @ Rs. 3.00 = 900 paise
    assert_eq!(calculate_job_cost_paise(2, 3, false, true, &rules), 900);
}

#[test]
fn bw_odd_duplex_charges_trailing_single_side_at_simplex_rate() {
    let rules = PricingRules::default();
    // 5 sides = 2 duplex sheets (600) + 1 single sheet (200) = 800 paise
    assert_eq!(calculate_job_cost_paise(5, 1, false, true, &rules), 800);
    // 1 side in duplex mode falls back to 1 single sheet = 200 paise
    assert_eq!(calculate_job_cost_paise(1, 1, false, true, &rules), 200);
}

#[test]
fn multi_copy_odd_duplex_starts_each_copy_on_fresh_sheet() {
    let rules = PricingRules::default();
    // 3-page B&W x 2 copies: per copy = 1 duplex (300) + 1 single (200) = 500 -> x2 = 1000 paise
    assert_eq!(calculate_job_cost_paise(3, 2, false, true, &rules), 1000);
    // 1-page B&W x 2 copies: per copy = 1 single (200) -> x2 = 400 paise (never 1 duplex = 300)
    assert_eq!(calculate_job_cost_paise(1, 2, false, true, &rules), 400);
    // 3-page Color x 2 copies: per copy = 1 duplex (1800) + 1 single (1000) = 2800 -> x2 = 5600 paise
    assert_eq!(calculate_job_cost_paise(3, 2, true, true, &rules), 5600);
}

#[test]
fn color_simplex_and_odd_duplex_rates() {
    let rules = PricingRules::default();
    // 2 pages x 1 copy Color simplex = 2 * 1000 = 2000 paise
    assert_eq!(calculate_job_cost_paise(2, 1, true, false, &rules), 2000);
    // 4 pages Color even duplex = 2 * 1800 = 3600 paise
    assert_eq!(calculate_job_cost_paise(4, 1, true, true, &rules), 3600);
    // 5 pages Color odd duplex = 2 * 1800 + 1 * 1000 = 4600 paise
    assert_eq!(calculate_job_cost_paise(5, 1, true, true, &rules), 4600);
}

#[test]
fn bw_bulk_slab_triggers_at_threshold_sides() {
    let rules = PricingRules::default();
    // 49 sides B&W simplex stays at normal rate: 49 * 200 = 9800 paise
    assert_eq!(calculate_job_cost_paise(49, 1, false, false, &rules), 9800);
    // 50 sides B&W triggers bulk slab: 50 * 100 = 5000 paise (whether simplex or duplex)
    assert_eq!(calculate_job_cost_paise(50, 1, false, false, &rules), 5000);
    assert_eq!(calculate_job_cost_paise(25, 2, false, true, &rules), 5000);
}

#[test]
fn color_jobs_ignore_bw_bulk_slab() {
    let rules = PricingRules::default();
    // 50 sides Color simplex remains 50 * 1000 = 50000 paise
    assert_eq!(calculate_job_cost_paise(50, 1, true, false, &rules), 50_000);
    // 51 sides Color duplex = 25 * 1800 + 1 * 1000 = 46000 paise
    assert_eq!(calculate_job_cost_paise(51, 1, true, true, &rules), 46_000);
}

#[test]
fn models_status_roundtrip_and_serde_json() {
    for (status, expected) in [
        (SessionStatus::Active, "ACTIVE"),
        (SessionStatus::PaidUpi, "PAID_UPI"),
        (SessionStatus::PaidCash, "PAID_CASH"),
        (SessionStatus::Khata, "KHATA"),
        (SessionStatus::Void, "VOID"),
    ] {
        assert_eq!(status.as_str(), expected);
        assert_eq!(SessionStatus::from_str(expected).unwrap(), status);
    }

    let err = SessionStatus::from_str("UNKNOWN").unwrap_err();
    assert!(matches!(err, LedgerError::InvalidStatus(s) if s == "UNKNOWN"));

    let json_err: LedgerError = serde_json::from_str::<PricingRules>("{bad}")
        .unwrap_err()
        .into();
    assert!(matches!(json_err, LedgerError::Serialization(_)));

    let job = IncomingPrintJob {
        windows_job_id: 11,
        event_record_id: Some(318),
        printer_name: "ShopTest_PCL6".to_owned(),
        doc_name: "test_doc.pdf".to_owned(),
        raw_pages: 4,
        raw_copies: 3,
        effective_pages: 12,
        is_duplex: true,
        is_color: false,
        duplex_mode: DuplexMode::DuplexLongEdge,
        color_mode: ColorMode::Monochrome,
        driver_anomaly_flag: None,
        captured_at: "2026-11-26T14:00:00Z".to_owned(),
    };
    let json = serde_json::to_string(&job).unwrap();
    let decoded: IncomingPrintJob = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, job);

    // Explicitly verify DuplexMode and ColorMode pricing & Unknown fallback policies
    assert_eq!(
        calculate_job_cost_for_modes(4, 1, ColorMode::Monochrome, DuplexMode::DuplexLongEdge, &PricingRules::default()),
        600
    );
    assert_eq!(
        calculate_job_cost_for_modes(4, 1, ColorMode::Monochrome, DuplexMode::DuplexShortEdge, &PricingRules::default()),
        600
    );
    // DuplexMode::Unknown bills as simplex safely (4 * 200 = 800 paise)
    assert_eq!(
        calculate_job_cost_for_modes(4, 1, ColorMode::Unknown, DuplexMode::Unknown, &PricingRules::default()),
        800
    );

    let summary = SessionSummary {
        session_id: 1,
        status: SessionStatus::PaidUpi,
        started_at: "2026-11-26T14:00:00Z".to_owned(),
        closed_at: Some("2026-11-26T14:05:00Z".to_owned()),
        job_count: 1,
        bw_pages: 12,
        color_pages: 0,
        jam_deducted_pages: 0,
        bw_jam_deducted_pages: 0,
        color_jam_deducted_pages: 0,
        bw_jam_rate_paise: 200,
        color_jam_rate_paise: 1000,
        addons_total_paise: 0,
        total_amount_paise: 1800,
        has_anomaly: false,
        unverified_jobs_count: 0,
        anomaly_reasons: vec![],
        jobs: vec![],
        last_doc_name: Some("test_doc.pdf".to_owned()),
    };
    let summary_json = serde_json::to_string(&summary).unwrap();
    assert!(summary_json.contains("\"PAID_UPI\""));
    let summary_decoded: SessionSummary = serde_json::from_str(&summary_json).unwrap();
    assert_eq!(summary_decoded, summary);
}
