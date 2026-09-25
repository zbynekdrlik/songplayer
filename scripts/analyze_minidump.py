#!/usr/bin/env python3
# Verified against the `minidump` PyPI package version 0.0.24 (pure Python, MIT).
# API surface used, confirmed from the installed package source:
#   from minidump.minidumpfile import MinidumpFile
#   mf = MinidumpFile.parse(path)
#   mf.exception.exception_records[i].{ThreadId, ExceptionRecord}
#       ExceptionRecord.{ExceptionCode_raw, ExceptionAddress, NumberParameters, ExceptionInformation}
#   mf.threads.threads[i].{ThreadId, Stack, ContextObject}    # ContextObject.Rip/.Rsp on AMD64
#   mf.modules.modules[i].{name, baseaddress, size, endaddress, timestamp}
#   mf.sysinfo.{ProcessorArchitecture, MajorVersion, MinorVersion, BuildNumber, ...}
#   mf.misc_info.{ProcessId, ProcessCreateTime, ...}          # accessors read defensively
#   mf.header.Flags
#   reader = mf.get_reader(); reader.memory_segments[i].{start_virtual_address, size,
#       end_virtual_address, inrange(addr)}; reader.read(virt_addr, size)
#
# Stand-alone forensic reader for a Windows minidump (WER LocalDumps, DumpType=1).
# Prints a fixed-section text report. The `minidump` package is imported LAZILY
# inside the parse path so this module imports with the stdlib alone (the CI
# eval-checks runner has numpy+soundfile only and unit-tests the pure helpers).
#
# Usage: python analyze_minidump.py <file.dmp> [--max-frames 64]
"""Analyze a Windows minidump: exception record, faulting-thread return-address
scan, module list, and system/misc streams — no debugger, no symbols required."""

from __future__ import annotations

import argparse
import sys

MINIDUMP_PIN = "minidump==0.0.24"

# STATUS_STACK_BUFFER_OVERRUN (__fastfail). ExceptionInformation[0] carries the
# FAST_FAIL_* subcode. Table from winnt.h (values without a Windows-defined name,
# e.g. 15-17, fall through to UNKNOWN(n)).
_FASTFAIL = {
    0: "LEGACY_GS_VIOLATION",
    1: "VTGUARD_CHECK_FAILURE",
    2: "STACK_COOKIE_CHECK_FAILURE",
    3: "CORRUPT_LIST_ENTRY",
    4: "INCORRECT_STACK",
    5: "INVALID_ARG",
    6: "GS_COOKIE_INIT",
    7: "FATAL_APP_EXIT",
    8: "RANGE_CHECK_FAILURE",
    9: "UNSAFE_REGISTRY_ACCESS",
    10: "GUARD_ICALL_CHECK_FAILURE",
    11: "GUARD_WRITE_CHECK_FAILURE",
    12: "INVALID_FIBER_SWITCH",
    13: "INVALID_SET_OF_CONTEXT",
    14: "INVALID_REFERENCE_COUNT",
    18: "INVALID_JUMP_BUFFER",
    19: "MRDATA_MODIFIED",
    20: "CERTIFICATION_FAILURE",
    21: "INVALID_EXCEPTION_CHAIN",
    22: "CRYPTO_LIBRARY",
    23: "INVALID_CALL_IN_DLL_CALLOUT",
    24: "INVALID_IMAGE_BASE",
    25: "DLOAD_PROTECTION_FAILURE",
    26: "UNSAFE_EXTENSION_CALL",
    27: "DEPRECATED_SERVICE_INVOKED",
    28: "INVALID_BUFFER_ACCESS",
    29: "INVALID_BALANCED_TREE",
    30: "INVALID_NEXT_THREAD",
    31: "GUARD_ICALL_CHECK_SUPPRESSED",
    32: "APCS_DISABLED",
    33: "INVALID_IDLE_STATE",
    34: "MRDATA_PROTECTION_FAILURE",
    35: "UNEXPECTED_HEAP_EXCEPTION",
    36: "INVALID_LOCK_STATE",
    37: "GUARD_JUMPTABLE",
    38: "INVALID_LONGJUMP_TARGET",
    39: "INVALID_DISPATCH_CONTEXT",
    40: "INVALID_THREAD",
    41: "INVALID_SYSCALL_NUMBER",
    42: "INVALID_FILE_OPERATION",
    43: "LPAC_ACCESS_DENIED",
    44: "RESERVED_DYNAMIC_CODE",
    45: "INVALID_CONTROL_STACK",
    46: "SET_CONTEXT_DENIED",
    47: "INVALID_IAT",
    48: "HEAP_METADATA_CORRUPTION",
    49: "PAYLOAD_RESTRICTION_VIOLATION",
    50: "LOW_LABEL_ACCESS_DENIED",
    51: "ENCLAVE_CALL_FAILURE",
    52: "UNHANDLED_LSS_EXCEPTON",
    53: "ADMINLESS_ACCESS_DENIED",
    54: "UNEXPECTED_CALL",
    55: "CONTROL_INVALID_RETURN_ADDRESS",
    56: "UNEXPECTED_HOST_BEHAVIOR",
    57: "FLAGS_CORRUPTION",
    58: "VEH_CALLBACK_FAILURE",
    59: "FLAGS_CORRUPTION_CHECK_FAILURE",
}

FASTFAIL_EXCEPTION_CODE = 0xC0000409


# ---------------------------------------------------------------------------
# Pure helpers (unit-tested on Linux; no `minidump` dependency).
# ---------------------------------------------------------------------------
def fastfail_name(code: int) -> str:
    """Name of a FAST_FAIL_* subcode; unknown -> "UNKNOWN(<n>)"."""
    return _FASTFAIL.get(code, f"UNKNOWN({code})")


def _basename(path: str) -> str:
    """Basename of a Windows or POSIX path, without importing os/ntpath state."""
    return path.replace("\\", "/").rstrip("/").rsplit("/", 1)[-1]


def resolve(addr, modules):
    """Map an address to "<module basename>+0x<offset>" using a list of
    (name, base, size) tuples; None when the address is inside no module."""
    for name, base, size in modules:
        if base <= addr < base + size:
            return f"{_basename(name)}+0x{addr - base:x}"
    return None


def scan_return_addresses(words, modules, max_frames=64):
    """Given an iterable of stack words (ints), return the ones that fall inside
    a loaded module, resolved to "module+0xoffset", in order, capped at
    max_frames. This is the symbol-less stack: recognisable by module."""
    hits = []
    for word in words:
        resolved = resolve(word, modules)
        if resolved is not None:
            hits.append(resolved)
            if len(hits) >= max_frames:
                break
    return hits


# ---------------------------------------------------------------------------
# Report sections (need a parsed MinidumpFile).
# ---------------------------------------------------------------------------
def _module_tuples(mf):
    """[(name, base, size), ...] for resolve()/scan_return_addresses()."""
    mods = getattr(mf, "modules", None)
    if mods is None:
        return []
    out = []
    for mod in mods.modules:
        out.append((mod.name, mod.baseaddress, mod.size))
    return out


def _exception_record(mf):
    """The first exception stream record, or None."""
    exc = getattr(mf, "exception", None)
    if not exc:
        return None
    records = getattr(exc, "exception_records", None) or []
    return records[0] if records else None


def _section_exception(mf, lines):
    lines.append("== exception ==")
    stream = _exception_record(mf)
    if stream is None:
        lines.append("  (no exception stream in this dump)")
        return None
    rec = stream.ExceptionRecord
    code = getattr(rec, "ExceptionCode_raw", None)
    if code is None:
        code = int(getattr(rec, "ExceptionCode", 0))
    lines.append(f"  ExceptionCode:    0x{code & 0xFFFFFFFF:08x}")
    lines.append(f"  ExceptionAddress: 0x{rec.ExceptionAddress:016x}")
    lines.append(f"  ThreadId:         0x{stream.ThreadId:x} ({stream.ThreadId})")
    nparams = getattr(rec, "NumberParameters", 0) or 0
    info = list(getattr(rec, "ExceptionInformation", None) or [])
    lines.append(f"  NumberParameters: {nparams}")
    if info:
        lines.append(
            "  ExceptionInformation: "
            + ", ".join(f"0x{v:x}" for v in info[: max(nparams, 1)])
        )
    if (code & 0xFFFFFFFF) == FASTFAIL_EXCEPTION_CODE and info:
        subcode = info[0]
        lines.append(f"  fast-fail subcode: {subcode} -> {fastfail_name(subcode)}")
    return stream.ThreadId


def _faulting_thread(mf, thread_id):
    threads = getattr(mf, "threads", None)
    if threads is None:
        return None
    for thread in threads.threads:
        if thread.ThreadId == thread_id:
            return thread
    return None


def _section_faulting_thread(mf, thread_id, max_frames, lines):
    lines.append("")
    lines.append("== faulting thread ==")
    if thread_id is None:
        lines.append("  (no faulting thread — exception stream absent)")
        return
    thread = _faulting_thread(mf, thread_id)
    if thread is None:
        lines.append(f"  (thread 0x{thread_id:x} not found in thread list)")
        return
    ctx = getattr(thread, "ContextObject", None)
    if ctx is None:
        lines.append("  (no parsed thread context — non-AMD64 or missing)")
        return
    rip = getattr(ctx, "Rip", None)
    rsp = getattr(ctx, "Rsp", None)
    if rip is not None:
        rip_res = resolve(rip, _module_tuples(mf))
        lines.append(f"  Rip: 0x{rip:016x}" + (f"  ({rip_res})" if rip_res else ""))
    if rsp is not None:
        lines.append(f"  Rsp: 0x{rsp:016x}")

    if rsp is None:
        return
    modules = _module_tuples(mf)
    reader = mf.get_reader()
    segment = next((s for s in reader.memory_segments if s.inrange(rsp)), None)
    if segment is None:
        lines.append("  (stack memory for Rsp not present in the dump)")
        return
    # Read from Rsp up to the top of its stack segment, capped so a huge segment
    # never blows the read; 1 MiB of stack is far more than any frame chain.
    nbytes = min(segment.end_virtual_address - rsp, 1024 * 1024)
    nbytes -= nbytes % 8
    if nbytes <= 0:
        lines.append("  (empty stack window)")
        return
    data = reader.read(rsp, nbytes)
    words = (
        int.from_bytes(data[i : i + 8], "little") for i in range(0, len(data) - 7, 8)
    )
    frames = scan_return_addresses(words, modules, max_frames=max_frames)
    lines.append(
        f"  return-address scan (Rsp -> top of stack, in-module words, "
        f"cap {max_frames}):"
    )
    if not frames:
        lines.append("    (no in-module words found on the stack)")
    for idx, frame in enumerate(frames):
        lines.append(f"    #{idx:<3} {frame}")


def _section_modules(mf, lines):
    lines.append("")
    lines.append("== modules ==")
    modstream = getattr(mf, "modules", None)
    if modstream is None:
        lines.append("  (no module list stream)")
        return
    mods = list(modstream.modules)
    # SongPlayer.exe first, then the rest by base address.
    mods.sort(
        key=lambda m: (
            0 if _basename(m.name).lower() == "songplayer.exe" else 1,
            m.baseaddress,
        )
    )
    lines.append(f"  {'name':<32} {'base':>18} {'size':>12}  timestamp")
    for mod in mods:
        ts = getattr(mod, "timestamp", 0) or 0
        lines.append(
            f"  {_basename(mod.name):<32} 0x{mod.baseaddress:016x} "
            f"0x{mod.size:010x}  0x{ts:08x}"
        )


def _section_system_misc(mf, lines):
    lines.append("")
    lines.append("== system/misc ==")
    si = getattr(mf, "sysinfo", None)
    if si is not None:
        arch = getattr(si, "ProcessorArchitecture", None)
        arch_name = getattr(arch, "name", str(arch))
        lines.append(f"  ProcessorArchitecture: {arch_name}")
        lines.append(
            "  OS: "
            f"{getattr(si, 'MajorVersion', '?')}."
            f"{getattr(si, 'MinorVersion', '?')} "
            f"build {getattr(si, 'BuildNumber', '?')}"
        )
        npcpu = getattr(si, "NumberOfProcessors", None)
        if npcpu is not None:
            lines.append(f"  NumberOfProcessors: {npcpu}")
        csd = getattr(si, "CSDVersion", None)
        if csd:
            lines.append(f"  CSDVersion: {csd}")
    else:
        lines.append("  (no SystemInfo stream)")

    mi = getattr(mf, "misc_info", None)
    if mi is not None:
        pid = getattr(mi, "ProcessId", None)
        if pid is not None:
            lines.append(f"  ProcessId: {pid}")
        for field in ("ProcessCreateTime", "ProcessUserTime", "ProcessKernelTime"):
            val = getattr(mi, field, None)
            if val is not None:
                lines.append(f"  {field}: {val}")
    else:
        lines.append("  (no MiscInfo stream)")

    header = getattr(mf, "header", None)
    flags = getattr(header, "Flags", None) if header is not None else None
    if flags is not None:
        lines.append(f"  DumpFlags: {getattr(flags, 'name', flags)}")

    # System-wide memory (commit total/limit) only if the library parsed it.
    smi = getattr(mf, "system_memory_info", None)
    if smi is not None:
        for field in (
            "CommittedPages",
            "CommitLimit",
            "AvailablePages",
            "PhysicalTotalPages",
        ):
            val = getattr(smi, field, None)
            if val is not None:
                lines.append(f"  {field}: {val}")


def build_report(path: str, max_frames: int = 64) -> str:
    """Parse the dump and render the full fixed-section report."""
    # Lazy import so the module (and its pure helpers) load without `minidump`.
    from minidump.minidumpfile import MinidumpFile

    mf = MinidumpFile.parse(path)
    lines: list[str] = [f"# minidump report: {path}"]
    thread_id = _section_exception(mf, lines)
    _section_faulting_thread(mf, thread_id, max_frames, lines)
    _section_modules(mf, lines)
    _section_system_misc(mf, lines)
    return "\n".join(lines) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Analyze a Windows minidump without a debugger or symbols."
    )
    parser.add_argument("dump", help="path to the .dmp file")
    parser.add_argument(
        "--max-frames",
        type=int,
        default=64,
        help="cap on the number of in-module stack words printed (default 64)",
    )
    args = parser.parse_args(argv)
    sys.stdout.write(build_report(args.dump, args.max_frames))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
