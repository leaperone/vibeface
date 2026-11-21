"""AUTO-23 exact-head and clean-tree source CLI fixture.

Migration map: ``run-delivery-live.py:self_test_source_cli`` →
``tests.delivery.test_live_source:exact_head_and_clean_tree``.
"""

import os
from pathlib import Path
import shutil
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[3]


def exact_head_and_clean_tree(directory):
    """Verify the source CLI against an exact, clean checkout fixture."""
    repo = Path(directory) / "source-cli"
    scripts = repo / "scripts"
    scripts.mkdir(parents=True)
    for name in ("run-delivery-live.py", "release_component_identity.py", "release_format.py", "release_context.py"):
        shutil.copy2(ROOT / "scripts" / name, scripts / name)
    shutil.copytree(ROOT / "scripts/verification", scripts / "verification",
                    ignore=shutil.ignore_patterns("__pycache__"))

    control = repo / "crates/control/src/lib.rs"
    control.parent.mkdir(parents=True)
    shutil.copy2(ROOT / "crates/control/src/lib.rs", control)

    def git(*args):
        return subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args],
                              cwd=repo, check=True, capture_output=True, text=True)

    git("init", "-q")
    git("add", "scripts", "crates")
    git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
    expected = git("rev-parse", "HEAD").stdout.strip()

    environment = os.environ.copy()
    environment.pop("PYTHONDONTWRITEBYTECODE", None)
    environment.pop("PYTHONPYCACHEPREFIX", None)

    def verify(commit, succeeds):
        result = subprocess.run([sys.executable, str(scripts / "run-delivery-live.py"),
                                 "--verify-source-commit", commit], cwd=repo, env=environment,
                                capture_output=True, text=True)
        assert (result.returncode == 0) == succeeds, result.stderr

    verify(expected, True)
    assert not git("status", "--porcelain").stdout
    verify("0" * 40, False)
    extra = repo / "untracked.txt"
    extra.write_text("untracked source")
    verify(expected, False)
    extra.unlink()
    with (scripts / "release_format.py").open("a") as stream:
        stream.write("\n# changed source\n")
    verify(expected, False)
    git("checkout", "--", "scripts/release_format.py")
    with (scripts / "release_context.py").open("a") as stream:
        stream.write("\n# changed context\n")
    verify(expected, False)
    assert not list(repo.rglob("__pycache__"))
