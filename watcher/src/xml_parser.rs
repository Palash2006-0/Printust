//! Pure-Rust streaming XML parser (`quick-xml`) for Windows PrintService Events `307` and `805`.

use std::collections::HashMap;

use printust_core::ColorMode;
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use thiserror::Error;

/// Typed errors emitted by XML parsing, correlation, and Win32 capture layers.
#[derive(Debug, Error)]
pub enum WatcherError {
    #[error("XML parse error: {0}")]
    XmlParse(String),

    #[error("unsupported Windows PrintService EventID: {0}")]
    UnsupportedEventId(u32),

    #[error("Win32 API `{api}` failed: {message}")]
    Win32 { api: &'static str, message: String },

    #[error("correlator/subscription channel closed unexpectedly")]
    ChannelClosed,

    #[error("core ledger error: {0}")]
    Core(#[from] printust_core::LedgerError),
}

/// Extracted fields from Windows PrintService Operational Event `805` (`RenderJobDiag`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event805Data {
    pub record_id: i64,
    pub timestamp: String,
    pub job_id: u32,
    pub gdi_job_size: u64,
    pub copies: u32,
    pub color: ColorMode,
    pub icm_method: Option<u32>,
    pub x_res: Option<u32>,
    pub y_res: Option<u32>,
    pub quality: Option<u32>,
    pub tt_option: Option<u32>,
}

/// Extracted fields from Windows PrintService Operational Event `307` (`DocumentPrinted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event307Data {
    pub record_id: i64,
    pub timestamp: String,
    pub job_id: u32,
    pub doc_name: String,
    pub user_name: String,
    pub machine_name: String,
    pub printer_name: String,
    pub port_name: String,
    pub size_bytes: u64,
    pub pages_printed: u32,
}

/// Typed representation of a parsed Windows PrintService event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSpoolerEvent {
    RenderDiag805(Event805Data),
    DocumentPrinted307(Event307Data),
}

impl ParsedSpoolerEvent {
    /// Return the spooler `JobId` associated with this event.
    pub fn job_id(&self) -> u32 {
        match self {
            Self::RenderDiag805(e) => e.job_id,
            Self::DocumentPrinted307(e) => e.job_id,
        }
    }

    /// Return the monotonic Windows Event Log `EventRecordID`.
    pub fn record_id(&self) -> i64 {
        match self {
            Self::RenderDiag805(e) => e.record_id,
            Self::DocumentPrinted307(e) => e.record_id,
        }
    }
}

fn local_tag_name(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).into_owned()
}

fn capture_time_created_attr(
    tag: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    timestamp: &mut String,
) -> Result<(), WatcherError> {
    for attr_res in tag.attributes() {
        let attr = attr_res.map_err(|e| WatcherError::XmlParse(e.to_string()))?;
        let key = attr.key.local_name();
        if key.as_ref() == b"SystemTime" {
            let decoded = attr
                .decode_and_unescape_value(reader.decoder())
                .map_err(|e| WatcherError::XmlParse(e.to_string()))?;
            *timestamp = decoded.trim().to_owned();
        }
    }
    Ok(())
}

/// Parse a raw Windows Event Log XML string into either `RenderDiag805` or `DocumentPrinted307`.
///
/// Supports both live Windows positional tags (`<Param1>`..`<Param8>` under `<DocumentPrinted>`)
/// and named field aliases (`<JobId>`, `<PagesPrinted>`, `<PrinterName>`, etc.).
pub fn parse_spooler_event_xml(xml_text: &str) -> Result<ParsedSpoolerEvent, WatcherError> {
    let mut reader = Reader::from_str(xml_text);
    reader.config_mut().trim_text(true);
    reader.config_mut().check_end_names = true;

    let mut stack: Vec<String> = Vec::with_capacity(8);
    let mut buf: Vec<u8> = Vec::new();
    let mut current_text = String::new();

    let mut saw_system = false;
    let mut event_id: Option<u32> = None;
    let mut record_id: Option<i64> = None;
    let mut timestamp = String::new();
    let mut fields: HashMap<String, String> = HashMap::with_capacity(12);

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = local_tag_name(e.local_name().as_ref());
                if name == "System" && stack.len() == 1 {
                    saw_system = true;
                }
                if name == "TimeCreated" && stack.iter().any(|p| p == "System") {
                    capture_time_created_attr(&e, &reader, &mut timestamp)?;
                }
                current_text.clear();
                stack.push(name);
            }
            Ok(Event::Empty(e)) => {
                let name = local_tag_name(e.local_name().as_ref());
                if name == "TimeCreated" && stack.iter().any(|p| p == "System") {
                    capture_time_created_attr(&e, &reader, &mut timestamp)?;
                } else if stack.iter().any(|p| p == "DocumentPrinted" || p == "RenderJobDiag") {
                    fields.entry(name).or_default();
                }
            }
            Ok(Event::Text(e)) => {
                let text = e
                    .unescape()
                    .map_err(|err| WatcherError::XmlParse(err.to_string()))?;
                current_text.push_str(&text);
            }
            Ok(Event::CData(e)) => {
                let text = e
                    .decode()
                    .map_err(|err| WatcherError::XmlParse(err.to_string()))?;
                current_text.push_str(&text);
            }
            Ok(Event::End(e)) => {
                let closed_name = local_tag_name(e.local_name().as_ref());
                let trimmed = current_text.trim().to_owned();

                if stack.len() >= 2 && stack[stack.len() - 2] == "System" {
                    match closed_name.as_str() {
                        "EventID" => {
                            let parsed = trimmed.parse::<u32>().map_err(|err| {
                                WatcherError::XmlParse(format!("invalid EventID '{trimmed}': {err}"))
                            })?;
                            event_id = Some(parsed);
                        }
                        "EventRecordID" => {
                            let parsed = trimmed.parse::<i64>().map_err(|err| {
                                WatcherError::XmlParse(format!(
                                    "invalid EventRecordID '{trimmed}': {err}"
                                ))
                            })?;
                            record_id = Some(parsed);
                        }
                        _ => {}
                    }
                } else if stack.len() >= 2
                    && matches!(
                        stack[stack.len() - 2].as_str(),
                        "DocumentPrinted" | "RenderJobDiag"
                    )
                {
                    fields.insert(closed_name, trimmed);
                }

                current_text.clear();
                stack.pop();
            }
            Ok(Event::Eof) => break,
            Err(err) => return Err(WatcherError::XmlParse(err.to_string())),
            _ => {}
        }
        buf.clear();
    }

    if !saw_system {
        return Err(WatcherError::XmlParse(
            "missing <System> element in Windows Event XML".to_owned(),
        ));
    }

    let event_id =
        event_id.ok_or_else(|| WatcherError::XmlParse("missing <EventID> in <System>".to_owned()))?;
    let record_id = record_id.ok_or_else(|| {
        WatcherError::XmlParse("missing <EventRecordID> in <System>".to_owned())
    })?;
    if timestamp.is_empty() {
        return Err(WatcherError::XmlParse(
            "missing <TimeCreated SystemTime=.../> in <System>".to_owned(),
        ));
    }

    match event_id {
        805 => parse_805_fields(record_id, timestamp, &fields).map(ParsedSpoolerEvent::RenderDiag805),
        307 => parse_307_fields(record_id, timestamp, &fields)
            .map(ParsedSpoolerEvent::DocumentPrinted307),
        other => Err(WatcherError::UnsupportedEventId(other)),
    }
}

fn get_first<'a>(fields: &'a HashMap<String, String>, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| fields.get(*k).map(|s| s.as_str()))
        .map(str::trim)
}

fn parse_required_u32(
    fields: &HashMap<String, String>,
    keys: &[&str],
    label: &str,
) -> Result<u32, WatcherError> {
    let raw = get_first(fields, keys)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| WatcherError::XmlParse(format!("missing required field '{label}'")))?;
    raw.parse::<u32>()
        .map_err(|err| WatcherError::XmlParse(format!("invalid u32 for '{label}' ('{raw}'): {err}")))
}

fn parse_optional_u32(fields: &HashMap<String, String>, keys: &[&str]) -> Option<u32> {
    get_first(fields, keys).and_then(|v| v.parse::<u32>().ok())
}

fn parse_805_fields(
    record_id: i64,
    timestamp: String,
    fields: &HashMap<String, String>,
) -> Result<Event805Data, WatcherError> {
    let job_id = parse_required_u32(fields, &["JobId"], "RenderJobDiag/JobId")?;
    let gdi_job_size = get_first(fields, &["GdiJobSize"])
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let copies = parse_optional_u32(fields, &["Copies"]).unwrap_or(1).max(1);
    let color = match parse_optional_u32(fields, &["Color"]) {
        Some(1) => ColorMode::Monochrome,
        Some(2) => ColorMode::Color,
        _ => ColorMode::Unknown,
    };

    Ok(Event805Data {
        record_id,
        timestamp,
        job_id,
        gdi_job_size,
        copies,
        color,
        icm_method: parse_optional_u32(fields, &["ICMMethod"]),
        x_res: parse_optional_u32(fields, &["XRes"]),
        y_res: parse_optional_u32(fields, &["YRes"]),
        quality: parse_optional_u32(fields, &["Quality"]),
        tt_option: parse_optional_u32(fields, &["TTOption"]),
    })
}

fn parse_307_fields(
    record_id: i64,
    timestamp: String,
    fields: &HashMap<String, String>,
) -> Result<Event307Data, WatcherError> {
    let job_id = parse_required_u32(fields, &["JobId", "Param1"], "DocumentPrinted/JobId(Param1)")?;
    let pages_printed = parse_required_u32(
        fields,
        &["PagesPrinted", "Param8"],
        "DocumentPrinted/PagesPrinted(Param8)",
    )?;
    let printer_name = get_first(fields, &["PrinterName", "Param5"])
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            WatcherError::XmlParse("missing required field 'DocumentPrinted/PrinterName(Param5)'".to_owned())
        })?
        .to_owned();

    let doc_name = get_first(fields, &["DocName", "DocumentName", "Param2"])
        .unwrap_or("")
        .to_owned();
    let user_name = get_first(fields, &["User", "UserName", "Param3"])
        .unwrap_or("")
        .to_owned();
    let machine_name = get_first(fields, &["Machine", "MachineName", "Param4"])
        .unwrap_or("")
        .to_owned();
    let port_name = get_first(fields, &["PortName", "Param6"])
        .unwrap_or("")
        .to_owned();
    let size_bytes = get_first(fields, &["SizeBytes", "Param7"])
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    Ok(Event307Data {
        record_id,
        timestamp,
        job_id,
        doc_name,
        user_name,
        machine_name,
        printer_name,
        port_name,
        size_bytes,
        pages_printed,
    })
}
