#!/usr/bin/env python3
"""win_replace.py — replace a file that another process may hold OPEN (#184 round F2).

Box evidence 2026-09-23 (#184 comment 5795075881): the dub rebuild of video 344
failed at its final step with `[WinError 5] Access is denied: '…_dub.part.flac' ->
'…_dub.flac'`. The SP-dabing playlist had the video loaded (paused), so
SongPlayer's `stems/reader.rs` held `_dub.flac` open. `os.replace` on Windows is
`MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which cannot replace a target another
handle has open, even when that handle was opened with `FILE_SHARE_DELETE` (Rust
std's `File::open` shares READ|WRITE|DELETE).

A rename with POSIX semantics can: `SetFileInformationByHandle(FileRenameInfoEx,
FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS)` (Windows 10
1709+; the box is build 22631). The directory entry switches to the new file at
once. The open reader keeps reading the OLD data through its handle, and the next
open (the next video load) gets the new dub, so playback never has to stop. This
is the same call Rust std's `fs::rename` falls back to.

Everywhere else this is plain `os.replace`, which already has POSIX semantics.
`build_rename_info` is pure and unit-tested on Linux
(`scripts/tests/test_win_replace.py`). Shipped next to `dub_worker.py` by the Rust
worker (`dabing::worker::embedded_tool_scripts`). stdlib only.
"""

from __future__ import annotations

import ctypes
import os

# winbase.h: FILE_INFO_BY_HANDLE_CLASS::FileRenameInfoEx + its rename flags.
FILE_RENAME_INFO_EX_CLASS = 22
FILE_RENAME_FLAG_REPLACE_IF_EXISTS = 0x1
FILE_RENAME_FLAG_POSIX_SEMANTICS = 0x2

# CreateFileW arguments: open the SOURCE for rename only (DELETE access), sharing
# everything so a concurrent reader of the source never blocks it.
_DELETE = 0x00010000
_SYNCHRONIZE = 0x00100000
_FILE_SHARE_ALL = 0x1 | 0x2 | 0x4  # FILE_SHARE_READ | _WRITE | _DELETE
_OPEN_EXISTING = 3
_FILE_ATTRIBUTE_NORMAL = 0x80


class FILE_RENAME_INFO(ctypes.Structure):
    """`FILE_RENAME_INFO` (winbase.h), header of a variable-length struct.

    Explicit-width fields keep the Windows x64 layout on every host: ULONG is
    32-bit (`ctypes.c_ulong` is 64-bit on Linux) and WCHAR is 16-bit
    (`ctypes.c_wchar` is 32-bit on Linux). The Flags/ReplaceIfExists union is
    represented by its 32-bit `Flags` member, which is what `FileRenameInfoEx`
    reads. Layout: Flags@0, RootDirectory@8, FileNameLength@16, FileName@20,
    sizeof 24."""

    _fields_ = [
        ("Flags", ctypes.c_uint32),
        ("RootDirectory", ctypes.c_void_p),
        ("FileNameLength", ctypes.c_uint32),
        ("FileName", ctypes.c_uint16 * 1),
    ]


def build_rename_info(dst: str) -> tuple[ctypes.Array, int]:
    """Build the `FILE_RENAME_INFO` buffer that renames a handle's file to `dst`
    (an absolute path) with REPLACE_IF_EXISTS | POSIX_SEMANTICS and no root
    directory.

    `FileNameLength` is the UTF-16 byte length WITHOUT a terminator. The name is
    still followed by a NUL, as Rust std's rename does. Returns `(buffer, size)`
    where `size` = the `FileName` offset + name bytes + 2 (the size passed to
    `SetFileInformationByHandle`). Pure, so it is unit-tested on Linux."""
    if not dst:
        raise ValueError("rename target path is empty")
    name = dst.encode("utf-16-le")
    offset = FILE_RENAME_INFO.FileName.offset
    size = offset + len(name) + 2
    buf = ctypes.create_string_buffer(size)
    info = FILE_RENAME_INFO.from_buffer(buf)
    info.Flags = FILE_RENAME_FLAG_REPLACE_IF_EXISTS | FILE_RENAME_FLAG_POSIX_SEMANTICS
    info.RootDirectory = None
    info.FileNameLength = len(name)
    ctypes.memmove(ctypes.addressof(buf) + offset, name, len(name))
    return buf, size


def _posix_rename_windows(src: str, dst: str) -> None:
    """Rename `src` over `dst` with POSIX semantics (Windows only).

    Opens `src` for DELETE and calls `SetFileInformationByHandle(FileRenameInfoEx)`.
    Any failure raises `OSError` carrying the Win32 error and both paths, which
    maps to `PermissionError` for ERROR_ACCESS_DENIED just like `os.replace`. The
    handle is always closed."""
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    create_file = kernel32.CreateFileW
    create_file.argtypes = [
        wintypes.LPCWSTR,
        wintypes.DWORD,
        wintypes.DWORD,
        ctypes.c_void_p,
        wintypes.DWORD,
        wintypes.DWORD,
        wintypes.HANDLE,
    ]
    create_file.restype = wintypes.HANDLE
    set_info = kernel32.SetFileInformationByHandle
    set_info.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    set_info.restype = wintypes.BOOL
    close_handle = kernel32.CloseHandle
    close_handle.argtypes = [wintypes.HANDLE]
    close_handle.restype = wintypes.BOOL

    def fail(what: str) -> OSError:
        err = ctypes.get_last_error()
        return OSError(0, f"{what}: {ctypes.FormatError(err)}", src, err, dst)

    handle = create_file(
        src,
        _DELETE | _SYNCHRONIZE,
        _FILE_SHARE_ALL,
        None,
        _OPEN_EXISTING,
        _FILE_ATTRIBUTE_NORMAL,
        None,
    )
    if handle is None or handle == ctypes.c_void_p(-1).value:
        raise fail("CreateFileW(DELETE) on the rename source failed")
    try:
        buf, size = build_rename_info(os.path.abspath(dst))
        if not set_info(handle, FILE_RENAME_INFO_EX_CLASS, buf, size):
            raise fail("SetFileInformationByHandle(FileRenameInfoEx, POSIX) failed")
    finally:
        close_handle(handle)


def replace_file(src: str, dst: str) -> None:
    """Atomically replace `dst` with `src`, even while another process holds `dst`
    open with FILE_SHARE_DELETE. Uses the POSIX-semantics rename on Windows and
    `os.replace` elsewhere. `os.name` is read at call time, so the dispatch is
    testable off Windows."""
    if os.name == "nt":
        _posix_rename_windows(src, dst)
    else:
        os.replace(src, dst)
