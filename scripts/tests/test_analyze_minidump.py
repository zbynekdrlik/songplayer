"""#156 — pure-helper tests for scripts/analyze_minidump.py.

The analyzer's report needs the `minidump` PyPI package, but its pure pieces —
`fastfail_name`, `resolve`, `scan_return_addresses` — carry the forensic logic
and are testable with the stdlib alone. `analyze_minidump.py` imports `minidump`
LAZILY inside `build_report`, so importing the module here does NOT pull it in,
and these tests run in the `eval-checks` CI job (numpy+soundfile only, no
`minidump`).
"""

import importlib.util
import os

_SCRIPTS_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def _load_analyzer():
    """Import scripts/analyze_minidump.py by path. Its module body is
    import-safe: the only `minidump` import is inside `build_report`."""
    path = os.path.join(_SCRIPTS_DIR, "analyze_minidump.py")
    spec = importlib.util.spec_from_file_location("analyze_minidump", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


am = _load_analyzer()


# ---- fastfail_name --------------------------------------------------------
def test_fastfail_name_fatal_app_exit():
    assert am.fastfail_name(7) == "FATAL_APP_EXIT"


def test_fastfail_name_stack_cookie():
    assert am.fastfail_name(2) == "STACK_COOKIE_CHECK_FAILURE"


def test_fastfail_name_unknown():
    assert am.fastfail_name(99) == "UNKNOWN(99)"


def test_fastfail_name_reserved_gap_is_unknown():
    # 15/16/17 have no Windows-defined name -> UNKNOWN.
    assert am.fastfail_name(15) == "UNKNOWN(15)"


# ---- resolve --------------------------------------------------------------
_SONGPLAYER = ("C:\\Program Files\\SongPlayer\\SongPlayer.exe", 0x140000000, 0x2000000)


def test_resolve_inside_module():
    addr = 0x140000000 + 0x111EC51
    assert am.resolve(addr, [_SONGPLAYER]) == "SongPlayer.exe+0x111ec51"


def test_resolve_at_base_is_zero_offset():
    assert am.resolve(0x140000000, [_SONGPLAYER]) == "SongPlayer.exe+0x0"


def test_resolve_outside_every_module_is_none():
    # One byte past the module's end (base+size) resolves to nothing.
    assert am.resolve(0x140000000 + 0x2000000, [_SONGPLAYER]) is None
    assert am.resolve(0x7FFF00000000, [_SONGPLAYER]) is None


def test_resolve_picks_the_containing_module():
    mods = [
        ("C:\\Windows\\System32\\ntdll.dll", 0x7FFAB0000000, 0x200000),
        _SONGPLAYER,
    ]
    assert am.resolve(0x7FFAB0000010, mods) == "ntdll.dll+0x10"


# ---- scan_return_addresses ------------------------------------------------
def test_scan_returns_only_in_module_hits_in_order():
    mods = [
        _SONGPLAYER,
        ("C:\\Windows\\System32\\KERNELBASE.dll", 0x7FFAA0000000, 0x300000),
    ]
    words = [
        0x0,  # miss
        0x140000000 + 0x111EC51,  # SongPlayer hit
        0xDEADBEEF,  # miss
        0x7FFAA0001234,  # KERNELBASE hit
        0x140000000 + 0x10,  # SongPlayer hit
    ]
    assert am.scan_return_addresses(words, mods) == [
        "SongPlayer.exe+0x111ec51",
        "KERNELBASE.dll+0x1234",
        "SongPlayer.exe+0x10",
    ]


def test_scan_respects_max_frames():
    mods = [_SONGPLAYER]
    words = [0x140000000 + i for i in range(0, 100)]
    hits = am.scan_return_addresses(words, mods, max_frames=3)
    assert len(hits) == 3
    assert hits == [
        "SongPlayer.exe+0x0",
        "SongPlayer.exe+0x1",
        "SongPlayer.exe+0x2",
    ]


def test_scan_empty_is_empty():
    assert am.scan_return_addresses([], [_SONGPLAYER]) == []
    assert am.scan_return_addresses([0x1, 0x2, 0x3], [_SONGPLAYER]) == []
