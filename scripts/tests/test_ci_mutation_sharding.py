"""The mutation gate spreads each crate's mutants over every shard (ci.yml).

cargo-mutants' default `--sharding slice` gives shard k the k-th run of
consecutive mutants, and `--in-diff` lists them file by file. A sp-server
mutant costs ~5 min (its build + the slow sp-server suite) where a sp-core
one costs seconds, so a push with many of both put eight sp-server mutants
in one shard and ran it past the 20-min bound (#242, CI run 37956682432:
shards 50-56 cancelled while the sp-core shards took 0.8 min).
`--sharding round-robin` (mutant i on shard i % k) spreads them evenly.
"""

import re
from pathlib import Path

_CI = Path(__file__).resolve().parents[2] / ".github" / "workflows" / "ci.yml"


def _commands(text: str) -> list[str]:
    """Every shell command line, with backslash continuations joined."""
    joined = re.sub(r"\\\n\s*", " ", text)
    return [line.strip() for line in joined.split("\n")]


def test_every_sharded_mutants_command_shards_round_robin():
    sharded = [
        line
        for line in _commands(_CI.read_text(encoding="utf-8"))
        if "cargo mutants" in line and "--shard " in line
    ]
    assert len(sharded) == 2, f"the shard's list + run commands, found {sharded}"
    for line in sharded:
        assert "--sharding round-robin" in line, line
