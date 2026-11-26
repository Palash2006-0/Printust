"""Day 3 probe: inspect Duplex/Copies/Color via Win32 JOB_INFO_2 (DEVMODE) and .SHD files."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import struct
from typing import Any

import win32print

DEFAULT_PRINTER_NAME = "ShopTest_PCL6"
DEFAULT_SPOOL_DIR = Path(r"C:\Windows\System32\spool\PRINTERS")

DEVMODE_SPEC_VERSION = 0x0401
DEVMODE_MIN_SIZE = 220

# DEVMODEW struct offsets (bytes, verified against wingdi.h / PyDEVMODEW)
OFF_DM_DEVICE_NAME = 0
OFF_DM_SPEC_VERSION = 64
OFF_DM_DRIVER_VERSION = 66
OFF_DM_SIZE = 68
OFF_DM_DRIVER_EXTRA = 70
OFF_DM_PAPER_SIZE = 78
OFF_DM_COPIES = 86
OFF_DM_COLOR = 92
OFF_DM_DUPLEX = 94

SHD_SIGNATURES = {
    0x00004966: "SHADOWFILE_NT4",
    0x00004967: "SHADOWFILE_2 (Win2k/XP)",
    0x00004968: "SHADOWFILE_2 (Vista/10/11)",
}
SHD_CANDIDATE_OFFDEVMODE_SLOTS = (0x28, 0x2C, 0x30, 0x34, 0x38)

DUPLEX_LABELS = {
    1: "SIMPLEX",
    2: "DUPLEX_VERTICAL (Long-Edge)",
    3: "DUPLEX_HORIZONTAL (Short-Edge)",
}

COLOR_LABELS = {
    1: "MONOCHROME",
    2: "COLOR",
}

PAPER_LABELS = {
    1: "Letter",
    9: "A4",
}

JOB_STATUS_FLAGS = (
    (getattr(win32print, "JOB_STATUS_PAUSED", 0x0001), "PAUSED"),
    (getattr(win32print, "JOB_STATUS_ERROR", 0x0002), "ERROR"),
    (getattr(win32print, "JOB_STATUS_DELETING", 0x0004), "DELETING"),
    (getattr(win32print, "JOB_STATUS_SPOOLING", 0x0008), "SPOOLING"),
    (getattr(win32print, "JOB_STATUS_PRINTING", 0x0010), "PRINTING"),
    (getattr(win32print, "JOB_STATUS_OFFLINE", 0x0020), "OFFLINE"),
    (getattr(win32print, "JOB_STATUS_PAPEROUT", 0x0040), "PAPEROUT"),
    (getattr(win32print, "JOB_STATUS_PRINTED", 0x0080), "PRINTED"),
    (getattr(win32print, "JOB_STATUS_DELETED", 0x0100), "DELETED"),
    (getattr(win32print, "JOB_STATUS_BLOCKED_DEVQ", 0x0200), "BLOCKED_DEVQ"),
    (getattr(win32print, "JOB_STATUS_USER_INTERVENTION", 0x0400), "USER_INTERVENTION"),
    (getattr(win32print, "JOB_STATUS_RESTART", 0x0800), "RESTART"),
    (getattr(win32print, "JOB_STATUS_COMPLETE", 0x1000), "COMPLETE"),
)


def decode_duplex(value: int | None) -> str:
    """Format Win32 dmDuplex numeric enum into human-readable label."""
    if value is None:
        return "UNKNOWN (None)"
    label = DUPLEX_LABELS.get(value)
    return f"{value} -> {label}" if label else f"{value} -> UNKNOWN"


def decode_color(value: int | None) -> str:
    """Format Win32 dmColor numeric enum into human-readable label."""
    if value is None:
        return "UNKNOWN (None)"
    label = COLOR_LABELS.get(value)
    return f"{value} -> {label}" if label else f"{value} -> UNKNOWN"


def decode_paper_size(value: int | None) -> str:
    """Format Win32 dmPaperSize numeric enum (A4, Letter, or raw int)."""
    if value is None:
        return "UNKNOWN (None)"
    label = PAPER_LABELS.get(value)
    return f"{value} -> {label}" if label else str(value)


def decode_job_status(status: int) -> str:
    """Decode Win32 JOB_INFO_2 Status bitmask into readable flag names."""
    if status == 0:
        return "0 (NONE)"
    names = [name for bit, name in JOB_STATUS_FLAGS if bit and (status & bit)]
    return f"{status} ({'|'.join(names)})" if names else str(status)


@dataclass
class DevModeInfo:
    """Parsed subset of DEVMODEW fields relevant to print-shop billing."""

    device_name: str
    spec_version: int
    dm_size: int
    copies: int
    duplex: int
    color: int
    paper_size: int


@dataclass
class JobInfo2Record:
    """Captured Win32 JOB_INFO_2 summary for a queued print job."""

    job_id: int
    document: str
    user_name: str
    total_pages: int
    pages_printed: int
    size_bytes: int
    status: int
    devmode: DevModeInfo | None


@dataclass
class ShdRecord:
    """Parsed Windows spooler .SHD shadow file metadata and embedded DEVMODEW."""

    file_name: str
    file_size: int
    signature: int
    signature_label: str
    job_id: int | None
    devmode_offset: int | None
    locator_source: str
    devmode: DevModeInfo | None


def _decode_utf16z(raw: bytes) -> str:
    """Decode a NUL-terminated UTF-16LE fixed char buffer safely."""
    text = raw.decode("utf-16-le", errors="ignore")
    return text.split("\x00", 1)[0].strip()


def _is_valid_devmode_at(data: bytes, offset: int) -> bool:
    """Check whether offset points to a structurally valid DEVMODEW struct."""
    if offset < 0 or offset + DEVMODE_MIN_SIZE > len(data):
        return False
    spec_version, _drv_ver, dm_size, _drv_extra = struct.unpack_from(
        "<HHHH", data, offset + OFF_DM_SPEC_VERSION
    )
    if spec_version != DEVMODE_SPEC_VERSION or dm_size < DEVMODE_MIN_SIZE:
        return False
    if offset + dm_size > len(data):
        return False
    device_name = _decode_utf16z(data[offset : offset + 64])
    if not device_name or not all(ch.isprintable() for ch in device_name):
        return False
    copies = struct.unpack_from("<h", data, offset + OFF_DM_COPIES)[0]
    color = struct.unpack_from("<h", data, offset + OFF_DM_COLOR)[0]
    duplex = struct.unpack_from("<h", data, offset + OFF_DM_DUPLEX)[0]
    return copies >= 0 and color in (0, 1, 2) and duplex in (0, 1, 2, 3)


def parse_devmode_bytes(data: bytes, offset: int) -> DevModeInfo:
    """Unpack DEVMODEW fields from raw bytes starting at `offset`."""
    if not _is_valid_devmode_at(data, offset):
        raise ValueError(f"Invalid DEVMODEW struct at byte offset {offset}")
    device_name = _decode_utf16z(data[offset : offset + 64])
    spec_version = struct.unpack_from("<H", data, offset + OFF_DM_SPEC_VERSION)[0]
    dm_size = struct.unpack_from("<H", data, offset + OFF_DM_SIZE)[0]
    paper_size = struct.unpack_from("<h", data, offset + OFF_DM_PAPER_SIZE)[0]
    copies = struct.unpack_from("<h", data, offset + OFF_DM_COPIES)[0]
    color = struct.unpack_from("<h", data, offset + OFF_DM_COLOR)[0]
    duplex = struct.unpack_from("<h", data, offset + OFF_DM_DUPLEX)[0]
    return DevModeInfo(
        device_name=device_name,
        spec_version=spec_version,
        dm_size=dm_size,
        copies=copies,
        duplex=duplex,
        color=color,
        paper_size=paper_size,
    )


def locate_devmode_in_shd(data: bytes, signature: int) -> tuple[int | None, str]:
    """Locate DEVMODEW offset via header offDevMode candidates or validated scan."""
    if signature in (0x00004967, 0x00004968):
        for slot in SHD_CANDIDATE_OFFDEVMODE_SLOTS:
            if slot + 4 <= len(data):
                candidate = struct.unpack_from("<I", data, slot)[0]
                if _is_valid_devmode_at(data, candidate):
                    return candidate, f"header[0x{slot:02X}]"
            if slot + 8 <= len(data):
                candidate64 = struct.unpack_from("<Q", data, slot)[0]
                if _is_valid_devmode_at(data, candidate64):
                    return candidate64, f"header64[0x{slot:02X}]"

    limit = len(data) - DEVMODE_MIN_SIZE + 1
    for pos in range(0, max(limit, 0), 2):
        if _is_valid_devmode_at(data, pos):
            return pos, "validated_scan"
    return None, "not_found"


def parse_shd_bytes(file_name: str, data: bytes) -> ShdRecord:
    """Parse a Windows spooler .SHD binary file and its embedded DEVMODEW struct."""
    file_size = len(data)
    if file_size < 16:
        raise ValueError(f"{file_name}: file too small ({file_size} bytes)")

    signature = struct.unpack_from("<I", data, 0)[0]
    sig_label = SHD_SIGNATURES.get(signature, "UNKNOWN")

    job_id: int | None = None
    stem = Path(file_name).stem
    if stem.isdigit():
        job_id = int(stem)
    elif signature in (0x00004967, 0x00004968):
        header_job_id = struct.unpack_from("<I", data, 0x0C)[0]
        if header_job_id > 0:
            job_id = header_job_id

    devmode_offset, locator_source = locate_devmode_in_shd(data, signature)
    devmode = (
        parse_devmode_bytes(data, devmode_offset)
        if devmode_offset is not None
        else None
    )

    return ShdRecord(
        file_name=file_name,
        file_size=file_size,
        signature=signature,
        signature_label=sig_label,
        job_id=job_id,
        devmode_offset=devmode_offset,
        locator_source=locator_source,
        devmode=devmode,
    )


def _extract_pydevmode(p_devmode: Any) -> DevModeInfo | None:
    """Convert pywin32 PyDEVMODEW object into DevModeInfo safely."""
    if p_devmode is None:
        return None
    return DevModeInfo(
        device_name=str(getattr(p_devmode, "DeviceName", "") or ""),
        spec_version=int(getattr(p_devmode, "SpecVersion", 0) or 0),
        dm_size=int(getattr(p_devmode, "Size", 0) or 0),
        copies=int(getattr(p_devmode, "Copies", 0) or 0),
        duplex=int(getattr(p_devmode, "Duplex", 0) or 0),
        color=int(getattr(p_devmode, "Color", 0) or 0),
        paper_size=int(getattr(p_devmode, "PaperSize", 0) or 0),
    )


def enum_printer_jobs_level2(printer_name: str) -> tuple[int, list[JobInfo2Record]]:
    """Query printer status and JOB_INFO_2 records via win32print.EnumJobs."""
    handle = win32print.OpenPrinter(printer_name)
    try:
        printer_info = win32print.GetPrinter(handle, 2)
        printer_status = int(printer_info.get("Status", 0) or 0)
        raw_jobs = win32print.EnumJobs(handle, 0, -1, 2)
    finally:
        win32print.ClosePrinter(handle)

    records: list[JobInfo2Record] = []
    for job in raw_jobs:
        records.append(
            JobInfo2Record(
                job_id=int(job.get("JobId", 0)),
                document=str(job.get("pDocument") or ""),
                user_name=str(job.get("pUserName") or ""),
                total_pages=int(job.get("TotalPages", 0) or 0),
                pages_printed=int(job.get("PagesPrinted", 0) or 0),
                size_bytes=int(job.get("Size", 0) or 0),
                status=int(job.get("Status", 0) or 0),
                devmode=_extract_pydevmode(job.get("pDevMode")),
            )
        )
    return printer_status, records


def scan_shd_files(spool_dir: Path) -> tuple[list[ShdRecord], list[str], str | None]:
    """Read and parse all *.SHD files in the Windows spool directory.

    Returns (parsed_records, per_file_errors, directory_error).
    """
    records: list[ShdRecord] = []
    file_errors: list[str] = []
    try:
        shd_paths = sorted(
            p for p in spool_dir.iterdir() if p.is_file() and p.suffix.lower() == ".shd"
        )
    except PermissionError as exc:
        return [], [], (
            f"PermissionError listing '{spool_dir}': {exc}. "
            "Re-run PowerShell as Administrator to list PRINTERS "
            "(note: individual .SHD files may still remain locked by spoolsv.exe)."
        )
    except FileNotFoundError as exc:
        return [], [], f"Spool directory not found: {exc}"

    for path in shd_paths:
        try:
            data = path.read_bytes()
            records.append(parse_shd_bytes(path.name, data))
        except PermissionError as exc:
            file_errors.append(
                f"{path.name}: PermissionError ({exc}) — file locked by spoolsv.exe / SYSTEM ACL"
            )
        except (OSError, ValueError) as exc:
            file_errors.append(f"{path.name}: {type(exc).__name__} ({exc})")

    return records, file_errors, None


def print_probe_report(
    printer_name: str,
    printer_status: int,
    jobs: list[JobInfo2Record],
    shd_records: list[ShdRecord],
    shd_file_errors: list[str],
    shd_dir_error: str | None,
) -> None:
    """Print Method 1 (JOB_INFO_2), Method 2 (.SHD), and Side-by-Side Comparison."""
    paused_bit = getattr(win32print, "PRINTER_STATUS_PAUSED", 0x0001)
    is_paused = bool(printer_status & paused_bit)

    print("=" * 78)
    print(
        f"Method 1: Win32 Spooler API (EnumJobs Level 2) — Printer={printer_name!r} "
        f"| Paused={is_paused}"
    )
    print("=" * 78)
    if not is_paused:
        print(
            "  [NOTE] Printer is not currently paused. Pause 'ShopTest_PCL6' "
            "(Queue -> Pause Printing) before printing so jobs stay queued."
        )
    if not jobs:
        print("  (No queued jobs found via EnumJobs Level 2.)")
    for job in jobs:
        print(
            f"  JobId={job.job_id} | Doc={job.document!r} | User={job.user_name!r} | "
            f"TotalPages={job.total_pages} | PagesPrinted={job.pages_printed} | "
            f"Size={job.size_bytes} | Status={decode_job_status(job.status)}"
        )
        if job.devmode is None:
            print("    pDevMode     : None (driver/spooler omitted DEVMODE)")
        else:
            dm = job.devmode
            print(
                f"    pDevMode     : Copies={dm.copies} | Duplex={decode_duplex(dm.duplex)} | "
                f"Color={decode_color(dm.color)} | PaperSize={decode_paper_size(dm.paper_size)}"
            )

    print("\n" + "=" * 78)
    print("Method 2: Direct .SHD Shadow File Inspection")
    print("=" * 78)
    if shd_dir_error:
        print(f"  [ACCESS NOTICE] {shd_dir_error}")
    elif not shd_records and not shd_file_errors:
        print("  (No .SHD files found in spool directory.)")
    else:
        for shd in shd_records:
            print(
                f"  File={shd.file_name} | Size={shd.file_size} B | "
                f"Sig=0x{shd.signature:08X} ({shd.signature_label}) | "
                f"JobId={shd.job_id}"
            )
            if shd.devmode is None:
                print(f"    DEVMODEW     : NOT FOUND ({shd.locator_source})")
            else:
                dm = shd.devmode
                print(
                    f"    DEVMODEW     : offset=0x{shd.devmode_offset:X} ({shd.locator_source}) | "
                    f"Spec=0x{dm.spec_version:04X} | dmSize={dm.dm_size} | "
                    f"Device={dm.device_name!r}"
                )
                print(
                    f"                   dmCopies={dm.copies} | "
                    f"dmDuplex={decode_duplex(dm.duplex)} | "
                    f"dmColor={decode_color(dm.color)} | "
                    f"dmPaperSize={decode_paper_size(dm.paper_size)}"
                )
        for err in shd_file_errors:
            print(f"  [SHD SKIPPED] {err}")

    print("\n" + "=" * 78)
    print("Side-by-Side Comparison (JOB_INFO_2 vs .SHD)")
    print("=" * 78)
    shd_by_job = {s.job_id: s for s in shd_records if s.job_id is not None}
    all_job_ids = sorted(
        {j.job_id for j in jobs} | {jid for jid in shd_by_job},
    )
    if not all_job_ids:
        print("  (Nothing to compare — pause ShopTest_PCL6 and send a test job first.)")
        return

    shd_blocked = bool(shd_dir_error or shd_file_errors)
    jobs_by_id = {j.job_id: j for j in jobs}
    for jid in all_job_ids:
        j = jobs_by_id.get(jid)
        s = shd_by_job.get(jid)
        j_dm = j.devmode if j else None
        s_dm = s.devmode if s else None

        j_str = (
            f"Copies={j_dm.copies}, Duplex={decode_duplex(j_dm.duplex)}, "
            f"Color={decode_color(j_dm.color)}, TotalPages={j.total_pages}"
            if j and j_dm
            else ("pDevMode=None" if j else "MISSING")
        )
        s_str = (
            f"dmCopies={s_dm.copies}, dmDuplex={decode_duplex(s_dm.duplex)}, "
            f"dmColor={decode_color(s_dm.color)}"
            if s and s_dm
            else (
                "DEVMODE=None"
                if s
                else ("UNAVAILABLE (Admin/File Lock)" if shd_blocked else "MISSING")
            )
        )

        verdict = "N/A"
        if j_dm and s_dm:
            matches = (
                j_dm.copies == s_dm.copies
                and j_dm.duplex == s_dm.duplex
                and j_dm.color == s_dm.color
            )
            verdict = "EXACT MATCH" if matches else "MISMATCH"
        elif j_dm and shd_blocked:
            verdict = "JOB_INFO_2 OK (.SHD locked/denied)"

        print(f"  JobId={jid}:")
        print(f"    JOB_INFO_2 : {j_str}")
        print(f"    .SHD       : {s_str}")
        print(f"    Verdict    : {verdict}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Inspect queued print jobs via Win32 JOB_INFO_2 and Windows .SHD files."
    )
    parser.add_argument(
        "--printer",
        default=DEFAULT_PRINTER_NAME,
        help=f"Target printer name (default: {DEFAULT_PRINTER_NAME}).",
    )
    parser.add_argument(
        "--spool-dir",
        type=Path,
        default=DEFAULT_SPOOL_DIR,
        help=f"Windows spool PRINTERS directory (default: {DEFAULT_SPOOL_DIR}).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    printer_status, jobs = enum_printer_jobs_level2(args.printer)
    shd_records, shd_file_errors, shd_dir_error = scan_shd_files(args.spool_dir)
    print_probe_report(
        args.printer, printer_status, jobs, shd_records, shd_file_errors, shd_dir_error
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
