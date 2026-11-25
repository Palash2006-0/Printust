"""Real-time Windows Print Spooler watcher and correlator for Event IDs 307 & 805."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
import time
from typing import Any
import xml.etree.ElementTree as ET

import win32evtlog

CHANNEL = "Microsoft-Windows-PrintService/Operational"
XPATH_FILTER = "*[System[(EventID=307 or EventID=805)]]"
POLL_INTERVAL_SECONDS = 0.4
EVENT_BATCH_SIZE = 32
PENDING_JOB_TTL_SECONDS = 15.0
LATE_805_MAX_RECORD_GAP = 2

SPIKE_DIR = Path(__file__).resolve().parent
REPO_ROOT = SPIKE_DIR.parent
RAW_XML_DIR = SPIKE_DIR / "raw_xml_samples"
EVENTS_JSONL_PATH = SPIKE_DIR / "events.jsonl"


def local_name(tag: str) -> str:
    """Strip XML namespace URI from an ElementTree tag name."""
    return tag.split("}", 1)[-1] if "}" in tag else tag


def close_evt_handle(handle: Any) -> None:
    """Safely close a Windows Event Log handle returned by pywin32."""
    if handle is None:
        return
    evt_close = getattr(win32evtlog, "EvtClose", None)
    if callable(evt_close):
        evt_close(handle)
        return
    close_fn = getattr(handle, "Close", None) or getattr(handle, "close", None)
    if callable(close_fn):
        close_fn()


def parse_int(value: Any) -> int | None:
    """Convert an integer-like string or value to int, returning None if invalid."""
    if value is None or isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    text = str(value).strip()
    if re.fullmatch(r"[+-]?\d+", text):
        return int(text)
    return None


def normalize_job_id(raw_value: Any) -> int | str | None:
    """Normalize JobId as int when numeric, stripped string otherwise, or None."""
    if raw_value is None:
        return None
    text = str(raw_value).strip()
    if not text:
        return None
    parsed = parse_int(text)
    return parsed if parsed is not None else text


def safe_filename_token(value: Any, fallback: str = "unknown") -> str:
    """Sanitize a value for safe inclusion in fixture filenames."""
    if value is None:
        return fallback
    cleaned = re.sub(r"[^A-Za-z0-9_-]", "_", str(value).strip())
    return cleaned or fallback


def _extract_element_value(element: ET.Element) -> Any:
    """Recursively extract leaf text or nested tag mappings without namespaces."""
    children = list(element)
    if not children:
        return (element.text or "").strip()
    nested: dict[str, Any] = {}
    for child in children:
        key = local_name(child.tag)
        val = _extract_element_value(child)
        if key in nested:
            existing = nested[key]
            if isinstance(existing, list):
                existing.append(val)
            else:
                nested[key] = [existing, val]
        else:
            nested[key] = val
    return nested


@dataclass
class ParsedPrintEvent:
    """Structured representation of a captured Windows PrintService event."""

    event_id: int
    record_id: int
    timestamp: str
    job_id: int | str | None
    userdata_section: str
    fields: dict[str, Any]
    raw_xml: str

    def event_key(self) -> tuple[int, str]:
        """Return a deduplication key resilient across event log resets."""
        return (self.record_id, self.timestamp)


@dataclass
class PendingJobEntry:
    """In-flight Event 805/307 correlation entry subject to completion eviction and TTL."""

    updated_at: float
    event_805: ParsedPrintEvent | None = None
    event_307: ParsedPrintEvent | None = None


def parse_event_xml(xml_text: str) -> ParsedPrintEvent:
    """Parse raw Windows Event XML (stripping namespaces) into a ParsedPrintEvent."""
    root = ET.fromstring(xml_text)

    system_el: ET.Element | None = None
    userdata_el: ET.Element | None = None
    eventdata_el: ET.Element | None = None

    for child in root:
        name = local_name(child.tag)
        if name == "System":
            system_el = child
        elif name == "UserData":
            userdata_el = child
        elif name == "EventData":
            eventdata_el = child

    if system_el is None:
        raise ValueError("Missing <System> element in Windows Event XML")

    event_id: int | None = None
    record_id: int | None = None
    timestamp = ""

    for child in system_el:
        name = local_name(child.tag)
        if name == "EventID":
            event_id = parse_int(child.text)
        elif name == "EventRecordID":
            record_id = parse_int(child.text)
        elif name == "TimeCreated":
            timestamp = child.attrib.get("SystemTime", "")

    if event_id is None or record_id is None:
        raise ValueError(
            f"Missing EventID ({event_id}) or EventRecordID ({record_id}) in <System>"
        )

    userdata_section = ""
    fields: dict[str, Any] = {}

    if userdata_el is not None:
        sections = list(userdata_el)
        if sections:
            section = sections[0]
            userdata_section = local_name(section.tag)
            extracted = _extract_element_value(section)
            if isinstance(extracted, dict):
                fields = extracted
            elif extracted != "":
                fields = {"value": extracted}
    elif eventdata_el is not None:
        userdata_section = "EventData"
        for idx, data_el in enumerate(eventdata_el, start=1):
            key = data_el.attrib.get("Name") or f"Data{idx}"
            fields[key] = (data_el.text or "").strip()

    raw_job_id = fields.get("JobId") if "JobId" in fields else fields.get("Param1")
    job_id = normalize_job_id(raw_job_id)

    return ParsedPrintEvent(
        event_id=event_id,
        record_id=record_id,
        timestamp=timestamp,
        job_id=job_id,
        userdata_section=userdata_section,
        fields=fields,
        raw_xml=xml_text,
    )


def extract_307_summary(fields: dict[str, Any]) -> dict[str, Any]:
    """Map Event 307 DocumentPrinted positional/named fields into readable attributes."""
    return {
        "job_id": normalize_job_id(fields.get("JobId") or fields.get("Param1")),
        "document_name": str(fields.get("DocumentName") or fields.get("Param2") or ""),
        "user": str(fields.get("User") or fields.get("Param3") or ""),
        "machine": str(fields.get("Machine") or fields.get("Param4") or ""),
        "printer_name": str(fields.get("PrinterName") or fields.get("Param5") or ""),
        "port_name": str(fields.get("PortName") or fields.get("Param6") or ""),
        "size_bytes": parse_int(fields.get("SizeBytes") or fields.get("Param7")),
        "pages_printed": parse_int(fields.get("PagesPrinted") or fields.get("Param8")),
    }


def detect_anomalies(
    summary_307: dict[str, Any], event_805: ParsedPrintEvent | None
) -> list[str]:
    """Detect missing 805 metadata or suspicious Copies vs PagesPrinted mismatches."""
    anomalies: list[str] = []
    if event_805 is None:
        anomalies.append(
            "[ANOMALY CHECK] Event 805 (RenderJobDiag) missing for this JobId."
        )
        return anomalies

    copies = parse_int(event_805.fields.get("Copies"))
    pages_printed = summary_307.get("pages_printed")

    if copies is not None and copies > 1:
        if pages_printed is None or pages_printed < copies or (pages_printed % copies != 0):
            anomalies.append(
                f"[ANOMALY CHECK] Copies={copies} > 1 while PagesPrinted={pages_printed} "
                f"looks unmultiplied!"
            )
        elif pages_printed == copies:
            anomalies.append(
                f"[ANOMALY CHECK] Copies={copies} > 1 and PagesPrinted={pages_printed} == Copies "
                f"(verify whether doc_pages > 1 was NOT multiplied by Copies)."
            )
    return anomalies


class SpoolerSpikeProcessor:
    """Correlates Event 805 & 307 jobs, exports XML fixtures, and logs JSONL records."""

    def __init__(
        self,
        raw_xml_dir: Path,
        events_jsonl_path: Path,
        pending_ttl_seconds: float = PENDING_JOB_TTL_SECONDS,
        repo_root: Path = REPO_ROOT,
    ) -> None:
        self.raw_xml_dir = raw_xml_dir
        self.events_jsonl_path = events_jsonl_path
        self.pending_ttl_seconds = pending_ttl_seconds
        self.repo_root = repo_root
        self.raw_xml_dir.mkdir(parents=True, exist_ok=True)
        self.events_jsonl_path.parent.mkdir(parents=True, exist_ok=True)
        self.jobs: dict[int | str, PendingJobEntry] = {}
        self.captured_count = 0

    def _prune_stale_jobs(self, now: float) -> None:
        """Evict unmatched pending entries past TTL to prevent JobId reuse collisions."""
        expired = [
            jid
            for jid, entry in self.jobs.items()
            if (now - entry.updated_at) > self.pending_ttl_seconds
        ]
        for jid in expired:
            self.jobs.pop(jid, None)

    def _relative_xml_path(self, raw_xml_path: Path) -> str:
        """Format XML fixture path relative to repo root so local user paths are never logged."""
        for base in (self.repo_root, self.events_jsonl_path.parent):
            try:
                return raw_xml_path.relative_to(base).as_posix()
            except ValueError:
                continue
        return raw_xml_path.name

    def _save_raw_xml(self, event: ParsedPrintEvent) -> Path:
        job_token = safe_filename_token(event.job_id)
        filename = f"event_{event.event_id}_rec_{event.record_id}_job_{job_token}.xml"
        out_path = self.raw_xml_dir / filename
        out_path.write_text(event.raw_xml, encoding="utf-8")
        return out_path

    def _append_jsonl(
        self,
        event: ParsedPrintEvent,
        raw_xml_path: Path,
        correlated_805: ParsedPrintEvent | None = None,
        anomalies: list[str] | None = None,
    ) -> None:
        payload: dict[str, Any] = {
            "timestamp": event.timestamp,
            "record_id": event.record_id,
            "event_id": event.event_id,
            "job_id": event.job_id,
            "userdata_section": event.userdata_section,
            "fields": event.fields,
            "raw_xml_path": self._relative_xml_path(raw_xml_path),
        }
        if event.event_id == 307:
            payload["event_805"] = (
                {
                    "record_id": correlated_805.record_id,
                    "timestamp": correlated_805.timestamp,
                    "fields": correlated_805.fields,
                }
                if correlated_805 is not None
                else None
            )
            payload["anomalies"] = anomalies or []

        with self.events_jsonl_path.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(payload, ensure_ascii=False) + "\n")

    def _print_correlated_summary(
        self,
        event_307: ParsedPrintEvent,
        event_805: ParsedPrintEvent | None,
        late_805: bool = False,
    ) -> list[str]:
        s307 = extract_307_summary(event_307.fields)
        anomalies = detect_anomalies(s307, event_805)

        header = (
            "[CORRELATED UPDATE (LATE 805)]"
            if late_805
            else "[CORRELATED JOB SUMMARY]"
        )
        copies_val = event_805.fields.get("Copies") if event_805 else "MISSING"
        color_val = event_805.fields.get("Color") if event_805 else "MISSING"
        gdi_size_val = event_805.fields.get("GdiJobSize") if event_805 else "MISSING"

        print("=" * 72)
        print(
            f"{header} JobId={event_307.job_id} | "
            f"Printer={s307['printer_name']!r} | Port={s307['port_name']!r}"
        )
        print(f"  Document        : {s307['document_name']!r} (User: {s307['user']})")
        print(
            f"  Event 307       : PagesPrinted={s307['pages_printed']} | "
            f"SizeBytes={s307['size_bytes']} | RecordId={event_307.record_id}"
        )
        if event_805 is not None:
            print(
                f"  Event 805       : Copies={copies_val} | Color={color_val} | "
                f"GdiJobSize={gdi_size_val} | RecordId={event_805.record_id}"
            )
            extra_805 = {
                k: v
                for k, v in event_805.fields.items()
                if k not in {"JobId", "Copies", "Color", "GdiJobSize"}
            }
            if extra_805:
                print(f"  Event 805 Extra : {json.dumps(extra_805, ensure_ascii=False)}")
        else:
            print("  Event 805       : null (not captured before Event 307)")

        for flag in anomalies:
            print(f"  {flag}")
        print("=" * 72, flush=True)
        return anomalies

    def handle_event(self, event: ParsedPrintEvent) -> None:
        """Persist raw XML + JSONL, correlate JobId, and evict completed entries."""
        now = time.monotonic()
        self._prune_stale_jobs(now)
        self.captured_count += 1
        raw_xml_path = self._save_raw_xml(event)

        if event.event_id == 805:
            self._append_jsonl(event, raw_xml_path)
            copies = event.fields.get("Copies", "?")
            color = event.fields.get("Color", "?")
            gdi_size = event.fields.get("GdiJobSize", "?")
            print(
                f"[805 RenderJobDiag] RecordId={event.record_id} JobId={event.job_id} "
                f"Copies={copies} Color={color} GdiJobSize={gdi_size}",
                flush=True,
            )
            if event.job_id is not None:
                existing = self.jobs.get(event.job_id)
                if (
                    existing is not None
                    and existing.event_307 is not None
                    and abs(event.record_id - existing.event_307.record_id)
                    <= LATE_805_MAX_RECORD_GAP
                ):
                    self._print_correlated_summary(
                        existing.event_307, event, late_805=True
                    )
                    self.jobs.pop(event.job_id, None)
                else:
                    # Replace any stale incomplete entry for this JobId with the new 805.
                    self.jobs[event.job_id] = PendingJobEntry(
                        updated_at=now, event_805=event
                    )
            return

        if event.event_id == 307:
            cached_805: ParsedPrintEvent | None = None
            if event.job_id is not None:
                existing = self.jobs.get(event.job_id)
                if existing is not None and existing.event_805 is not None:
                    cached_805 = existing.event_805
                    # Evict immediately after correlation so JobId reuse cannot inherit stale state.
                    self.jobs.pop(event.job_id, None)
                else:
                    self.jobs[event.job_id] = PendingJobEntry(
                        updated_at=now, event_307=event
                    )
            anomalies = self._print_correlated_summary(event, cached_805, late_805=False)
            self._append_jsonl(
                event, raw_xml_path, correlated_805=cached_805, anomalies=anomalies
            )
            return

        self._append_jsonl(event, raw_xml_path)
        print(
            f"[Event {event.event_id}] RecordId={event.record_id} "
            f"JobId={event.job_id} Fields={event.fields}",
            flush=True,
        )


def query_baseline_keys(
    channel: str = CHANNEL, xpath: str = XPATH_FILTER
) -> set[tuple[int, str]]:
    """Record existing EventRecordID + SystemTime pairs on startup to suppress history."""
    seen: set[tuple[int, str]] = set()
    flags = win32evtlog.EvtQueryChannelPath | win32evtlog.EvtQueryForwardDirection
    query_handle = win32evtlog.EvtQuery(channel, flags, xpath, None)
    try:
        while True:
            handles = win32evtlog.EvtNext(query_handle, EVENT_BATCH_SIZE)
            if not handles:
                break
            try:
                for evt_handle in handles:
                    xml_text = win32evtlog.EvtRender(
                        evt_handle, win32evtlog.EvtRenderEventXml
                    )
                    parsed = parse_event_xml(xml_text)
                    seen.add(parsed.event_key())
            finally:
                for evt_handle in handles:
                    close_evt_handle(evt_handle)
    finally:
        close_evt_handle(query_handle)
    return seen


def poll_new_events(
    seen_keys: set[tuple[int, str]],
    channel: str = CHANNEL,
    xpath: str = XPATH_FILTER,
) -> list[ParsedPrintEvent]:
    """Poll newest events in reverse order and return unseen events in strict RecordID order."""
    fresh_reverse: list[ParsedPrintEvent] = []
    flags = win32evtlog.EvtQueryChannelPath | win32evtlog.EvtQueryReverseDirection
    query_handle = win32evtlog.EvtQuery(channel, flags, xpath, None)
    try:
        done = False
        while not done:
            handles = win32evtlog.EvtNext(query_handle, EVENT_BATCH_SIZE)
            if not handles:
                break
            try:
                for evt_handle in handles:
                    xml_text = win32evtlog.EvtRender(
                        evt_handle, win32evtlog.EvtRenderEventXml
                    )
                    parsed = parse_event_xml(xml_text)
                    key = parsed.event_key()
                    if key in seen_keys:
                        done = True
                        break
                    seen_keys.add(key)
                    fresh_reverse.append(parsed)
            finally:
                for evt_handle in handles:
                    close_evt_handle(evt_handle)
    finally:
        close_evt_handle(query_handle)

    fresh_reverse.sort(key=lambda ev: ev.record_id)
    return fresh_reverse


def watch_loop(
    poll_interval: float = POLL_INTERVAL_SECONDS, include_existing: bool = False
) -> int:
    """Run the real-time PrintService Operational watcher until Ctrl+C."""
    processor = SpoolerSpikeProcessor(RAW_XML_DIR, EVENTS_JSONL_PATH)
    seen_keys = set() if include_existing else query_baseline_keys()

    print(
        f"Watching {CHANNEL!r} (poll={poll_interval:.2f}s, "
        f"baseline_skipped={len(seen_keys)})...",
        flush=True,
    )
    print(f"Raw XML fixtures -> {RAW_XML_DIR}", flush=True)
    print(f"JSONL event log  -> {EVENTS_JSONL_PATH}", flush=True)
    print("Trigger a print job to 'ShopTest_PCL6'. Press Ctrl+C to stop.\n", flush=True)

    try:
        while True:
            new_events = poll_new_events(seen_keys)
            for event in new_events:
                processor.handle_event(event)
            time.sleep(poll_interval)
    except KeyboardInterrupt:
        print(
            f"\nShutting down cleanly. Captured {processor.captured_count} new event(s).",
            flush=True,
        )
        return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Real-time Windows Print Spooler Event ID 307 & 805 watcher."
    )
    parser.add_argument(
        "--poll-interval",
        type=float,
        default=POLL_INTERVAL_SECONDS,
        help=f"Polling interval in seconds (default: {POLL_INTERVAL_SECONDS}).",
    )
    parser.add_argument(
        "--include-existing",
        action="store_true",
        help="Process existing historical 307/805 events on startup instead of skipping baseline.",
    )
    return parser.parse_args()


if __name__ == "__main__":
    args = parse_args()
    raise SystemExit(
        watch_loop(
            poll_interval=args.poll_interval,
            include_existing=args.include_existing,
        )
    )
