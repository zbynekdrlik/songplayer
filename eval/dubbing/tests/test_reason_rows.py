"""Pure unit tests for the round-3 reason-row engines (fishaudio/s2-pro and
bosonai/higgs-*) — the structured blocking reasons that back their report rows.
No model, no network."""

from __future__ import annotations

from eval.dubbing.engines import fish_s2, higgs


def test_fish_s2_reason_is_non_commercial_and_not_runnable():
    r = fish_s2.blocking_reason()
    assert r["model"] == "fishaudio/s2-pro"
    assert r["slovak"] is True
    assert "non-commercial" in r["license"].lower()
    assert "commercial use" in r["license_quote"].lower()
    assert r["runnable_8gb"] is False


def test_fish_s2_engine_fails_loud():
    import pytest

    eng = fish_s2.FishS2Engine()
    with pytest.raises(RuntimeError, match="not rendered"):
        eng.clone_voice("/tmp/x.wav")
    with pytest.raises(RuntimeError, match="not rendered"):
        eng.synthesize("t", "/tmp/x.wav")


def test_higgs_reason_weights_exceed_vram():
    r = higgs.blocking_reason()
    assert r["slovak"] is True
    assert (
        "non-commercial" in r["license"].lower()
        or "non-commercial" in r["license_quote"].lower()
    )
    # 7.91 GiB weights leave only ~55 MiB below the 7.96 GiB card — far under the
    # CUDA-context floor, so it cannot load.
    assert r["weight_bytes"] == 8_489_763_794
    assert r["headroom_mib"] < higgs.CUDA_CONTEXT_FLOOR_MIB
    assert r["runnable_8gb"] is False


def test_higgs_engine_fails_loud():
    import pytest

    eng = higgs.HiggsEngine()
    with pytest.raises(RuntimeError, match="not rendered"):
        eng.clone_voice("/tmp/x.wav")
    with pytest.raises(RuntimeError, match="not rendered"):
        eng.synthesize("t", "/tmp/x.wav")
