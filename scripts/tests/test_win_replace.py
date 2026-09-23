"""#184 round F2 — the dub promotion is a POSIX-semantics rename on Windows.

Box evidence 2026-09-23 12:44 UTC (#184 comment 5795075881): rebuilding video 344's
dub failed at the very last step with `[WinError 5] Access is denied:
'…_dub.part.flac' -> '…_dub.flac'`. The SP-dabing playlist had the video loaded
(paused), so SongPlayer held `_dub.flac` open. `os.replace` is
`MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which cannot replace a target another
handle holds open — even one opened with `FILE_SHARE_DELETE`. A rename with
`FileRenameInfoEx` + `FILE_RENAME_FLAG_POSIX_SEMANTICS` can: the open reader keeps
reading the old data, and the next open gets the new dub.

`scripts/win_replace.py` ships next to the worker. The eval-checks job is Linux,
so these tests pin the Win32 buffer layout the Windows path passes to
`SetFileInformationByHandle` (a pure builder), the `os.name` dispatch, and the
non-Windows `os.replace` behaviour on real files.
"""

import ctypes
import importlib.util
import os
import struct

import pytest

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load():
    path = os.path.join(_SCRIPTS_DIR, "win_replace.py")
    spec = importlib.util.spec_from_file_location("win_replace_under_test", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


wr = _load()


def test_rename_info_constants_are_the_win32_values():
    # winbase.h / WinBase FILE_INFO_BY_HANDLE_CLASS + FILE_RENAME_FLAG_*.
    assert wr.FILE_RENAME_INFO_EX_CLASS == 22
    assert wr.FILE_RENAME_FLAG_REPLACE_IF_EXISTS == 0x1
    assert wr.FILE_RENAME_FLAG_POSIX_SEMANTICS == 0x2


def test_rename_info_struct_matches_the_x64_abi():
    # FILE_RENAME_INFO on x64: ULONG Flags @0, 4 bytes padding, HANDLE
    # RootDirectory @8, DWORD FileNameLength @16, WCHAR FileName[1] @20, sizeof 24.
    # WCHAR is 16-bit on Windows (ctypes.c_wchar is 32-bit on Linux, so the struct
    # must not use it) and ULONG is 32-bit (ctypes.c_ulong is 64-bit on Linux).
    s = wr.FILE_RENAME_INFO
    assert ctypes.sizeof(ctypes.c_void_p) == 8
    assert s.Flags.offset == 0 and s.Flags.size == 4
    assert s.RootDirectory.offset == 8 and s.RootDirectory.size == 8
    assert s.FileNameLength.offset == 16 and s.FileNameLength.size == 4
    assert s.FileName.offset == 20 and s.FileName.size == 2
    assert ctypes.sizeof(s) == 24


@pytest.mark.parametrize(
    "dst",
    [
        r"C:\ProgramData\SongPlayer\cache\a_normalized_dub.flac",
        # Slovak diacritics + a non-BMP char (a UTF-16 surrogate pair = 4 bytes):
        # the length is counted in UTF-16 BYTES, never in characters.
        "C:\\cache\\Ježiš je Pán 🙏_normalized_dub.flac",
    ],
)
def test_build_rename_info_lays_out_flags_length_and_name(dst):
    buf, size = wr.build_rename_info(dst)
    raw = bytes(buf)
    s = wr.FILE_RENAME_INFO
    name = dst.encode("utf-16-le")

    (flags,) = struct.unpack_from("<I", raw, s.Flags.offset)
    (root,) = struct.unpack_from("<Q", raw, s.RootDirectory.offset)
    (name_len,) = struct.unpack_from("<I", raw, s.FileNameLength.offset)
    assert flags == 3  # REPLACE_IF_EXISTS | POSIX_SEMANTICS
    assert root == 0  # absolute target path, no root directory handle
    assert name_len == len(name)  # BYTES, without the terminating NUL
    off = s.FileName.offset
    assert raw[off : off + len(name)] == name
    # A terminating NUL follows the name (as Rust std's rename does) and the size
    # handed to SetFileInformationByHandle covers header + name + NUL.
    assert raw[off + len(name) : off + len(name) + 2] == b"\x00\x00"
    assert size == off + len(name) + 2
    assert len(raw) == size


def test_replace_file_off_windows_replaces_an_existing_target(tmp_path):
    src = tmp_path / "x_dub.part.flac"
    dst = tmp_path / "x_dub.flac"
    src.write_text("NEW", encoding="utf-8")
    dst.write_text("OLD", encoding="utf-8")

    wr.replace_file(str(src), str(dst))

    assert dst.read_text(encoding="utf-8") == "NEW"
    assert not src.exists()


def test_replace_file_off_windows_missing_source_raises(tmp_path):
    dst = tmp_path / "x_dub.flac"
    dst.write_text("OLD", encoding="utf-8")
    with pytest.raises(FileNotFoundError):
        wr.replace_file(str(tmp_path / "missing.part.flac"), str(dst))
    assert dst.read_text(encoding="utf-8") == "OLD"


def test_replace_file_on_windows_uses_the_posix_rename(tmp_path, monkeypatch):
    # `os.name` is read at CALL time, so the Windows dispatch is testable here;
    # the Win32 call itself is replaced by a recorder (no kernel32 on Linux).
    calls = []
    monkeypatch.setattr(
        wr, "_posix_rename_windows", lambda src, dst: calls.append((src, dst))
    )
    monkeypatch.setattr(wr.os, "name", "nt")
    wr.replace_file("C:\\c\\x_dub.part.flac", "C:\\c\\x_dub.flac")
    monkeypatch.undo()
    assert calls == [("C:\\c\\x_dub.part.flac", "C:\\c\\x_dub.flac")]


def test_replace_file_off_windows_never_touches_the_win32_path(tmp_path, monkeypatch):
    def boom(src, dst):
        raise AssertionError("the Win32 rename ran off Windows")

    monkeypatch.setattr(wr, "_posix_rename_windows", boom)
    monkeypatch.setattr(wr.os, "name", "posix")
    src = tmp_path / "a.part"
    dst = tmp_path / "a"
    src.write_text("NEW", encoding="utf-8")
    wr.replace_file(str(src), str(dst))
    assert dst.read_text(encoding="utf-8") == "NEW"
