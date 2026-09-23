"""#144 r3 — every ``wrapper.align(...)`` call in the mtl runner's
``align_fixture`` must execute inside ``torch.inference_mode()``.

Root cause (design record on #144): the upstream ``wrapper.align`` runs the
acoustic + boundary models' whole-song forwards with autograd ENABLED, so the
acoustic model's graph is retained while the boundary model runs; past ~4:15
an unchecked allocation inside torch's CPU conv2d faults the interpreter
(``c10.dll 0xc0000005``). Wrapping every ``align`` call in ``inference_mode()``
drops the graph and the crash (probe B: 170 s, peak WS 2.9 GB, no numeric
change).

This test runs in the ``eval-checks`` CI job, which installs ONLY
numpy+soundfile+jsonschema+requests — NO ``torch``. So it injects a FAKE
``torch`` module (an ``inference_mode()`` context manager that flips a
module-level flag, ``is_inference_mode_enabled()`` reads it) and a FAKE wrapper
(records the flag torch reports at each ``align`` call) into place, exactly as
``.claude/rules/lyrics-worker-tests.md`` prescribes for ``lyrics_worker.py``.
No real model or audio is touched.
"""

from __future__ import annotations

import importlib.util
import sys
import types
from pathlib import Path
from typing import Any

RUN_PY = (
    Path(__file__).resolve().parents[1] / "aligners" / "lyrics_alignment_mtl" / "run.py"
)


def _load_run_py() -> Any:
    """Load run.py by path — its ``aligners`` dir is not an importable
    package, and the module body imports only stdlib (``torch`` is imported
    lazily inside ``align_fixture``), so this pulls in no heavy dep."""
    spec = importlib.util.spec_from_file_location("mtl_run_under_test", RUN_PY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class _FakeInferenceCtx:
    """Stand-in for the guard ``torch.inference_mode()`` returns: on enter it
    marks the fake torch's inference flag active, on exit it restores the
    previous value (so nesting / sequential ``with`` blocks are honest)."""

    def __init__(self, torch_mod: Any) -> None:
        self._torch = torch_mod
        self._prev = False

    def __enter__(self) -> _FakeInferenceCtx:
        self._prev = self._torch.inference_active
        self._torch.inference_active = True
        return self

    def __exit__(self, *exc: object) -> bool:
        self._torch.inference_active = self._prev
        return False


class _FakeOutOfMemoryError(Exception):
    """Fake of ``torch.cuda.OutOfMemoryError`` — the class ``align_fixture``'s
    ``except`` clause names."""


def _make_fake_torch() -> types.ModuleType:
    torch_mod = types.ModuleType("torch")
    torch_mod.inference_active = False  # type: ignore[attr-defined]

    def _inference_mode() -> _FakeInferenceCtx:
        return _FakeInferenceCtx(torch_mod)

    def _is_inference_mode_enabled() -> bool:
        return bool(torch_mod.inference_active)  # type: ignore[attr-defined]

    torch_mod.inference_mode = _inference_mode  # type: ignore[attr-defined]
    torch_mod.is_inference_mode_enabled = _is_inference_mode_enabled  # type: ignore[attr-defined]
    torch_mod.cuda = types.SimpleNamespace(  # type: ignore[attr-defined]
        OutOfMemoryError=_FakeOutOfMemoryError,
        empty_cache=lambda: None,
    )
    return torch_mod


class _FakeWrapper:
    """Minimal stand-in for the upstream ``wrapper`` module. Records the
    inference-mode flag torch reports at each ``align`` call so the test can
    prove every call ran inside ``inference_mode()``."""

    def __init__(
        self,
        torch_mod: Any,
        *,
        words: list[str],
        raise_oom_on_first: bool = False,
    ) -> None:
        self._torch = torch_mod
        self._words = words
        self._raise_oom_on_first = raise_oom_on_first
        self.recorded_modes: list[bool] = []
        self.align_calls = 0

    def preprocess_from_file(
        self, wav_path: str, lyrics_file: str, word_file: Any = None
    ) -> tuple[Any, list[str], Any, Any, Any]:
        # Returns (audio, words, lyrics_p, idx_word_p, idx_line_p). ``words``
        # must equal run.build_line_word_map()'s filtered list or align_fixture
        # raises before ever calling align — so hand back the exact list.
        return (object(), list(self._words), object(), object(), object())

    def align(
        self,
        audio: Any,
        words: list[str],
        lyrics_p: Any,
        idx_word_p: Any,
        idx_line_p: Any,
        *,
        method: str,
        cuda: bool,
    ) -> tuple[list[list[int]], list[str]]:
        self.align_calls += 1
        self.recorded_modes.append(self._torch.is_inference_mode_enabled())
        if self._raise_oom_on_first and self.align_calls == 1:
            raise self._torch.cuda.OutOfMemoryError("simulated shared-GPU OOM")
        word_align = [[i * 10, i * 10 + 10] for i in range(len(words))]
        return word_align, list(words)


def _run_align_fixture(
    monkeypatch: Any, *, cuda: bool, raise_oom_on_first: bool = False
) -> _FakeWrapper:
    run = _load_run_py()
    fake_torch = _make_fake_torch()
    monkeypatch.setitem(sys.modules, "torch", fake_torch)

    lines_text = ["hello world"]
    filtered_words = ["hello", "world"]  # what build_line_word_map() yields
    wrapper = _FakeWrapper(
        fake_torch, words=filtered_words, raise_oom_on_first=raise_oom_on_first
    )
    monkeypatch.setattr(run, "install_compat_shims", lambda repo_dir: wrapper)

    run.align_fixture(
        wav_path=Path("/does-not-exist.wav"),
        lines_text=lines_text,
        lines_text_sk=[None],
        repo_dir="/does-not-exist-repo",
        cuda=cuda,
    )
    return wrapper


def test_cpu_branch_runs_under_inference_mode(monkeypatch: Any) -> None:
    wrapper = _run_align_fixture(monkeypatch, cuda=False)
    assert wrapper.align_calls == 1
    assert wrapper.recorded_modes == [
        True
    ], "the cpu-branch wrapper.align() must run inside torch.inference_mode()"


def test_cuda_branch_runs_under_inference_mode(monkeypatch: Any) -> None:
    wrapper = _run_align_fixture(monkeypatch, cuda=True)
    assert wrapper.align_calls == 1
    assert wrapper.recorded_modes == [
        True
    ], "the cuda-branch wrapper.align() must run inside torch.inference_mode()"


def test_cuda_oom_retry_runs_under_inference_mode(monkeypatch: Any) -> None:
    wrapper = _run_align_fixture(monkeypatch, cuda=True, raise_oom_on_first=True)
    # First (cuda) call raises OOM inside the context; the cpu retry runs in a
    # fresh context — BOTH must report inference mode active.
    assert wrapper.align_calls == 2
    assert wrapper.recorded_modes == [True, True], (
        "both the cuda call and its cpu OOM retry must run inside "
        "torch.inference_mode()"
    )
