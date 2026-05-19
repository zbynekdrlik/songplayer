"""Smoke test — confirms the eval/ pytest harness collects."""


def test_imports() -> None:
    import json  # noqa: F401
    import jsonschema  # noqa: F401

    assert True
