"""Run the CI self-test cases in the established CLI order."""

from functools import partial
import os
from pathlib import Path

from .coverage import changed_ranges_and_metadata
from .delivery import native_delivery_self_test
from .planner import planner_matrix, tool_commands
from .privacy import gatekeeper_test, release_privacy_test
from .support import ROOT
from .workflow import workflow_and_installer


def run_all():
    scratch = Path(os.environ.get("VIBEFACE_RELEASE_SCRATCH", ROOT / ".artifacts/ci-self-test"))
    scratch.mkdir(parents=True, exist_ok=True)
    cases = (
        planner_matrix,
        partial(native_delivery_self_test, scratch),
        partial(changed_ranges_and_metadata, scratch),
        tool_commands,
        workflow_and_installer,
        partial(release_privacy_test, ROOT, scratch),
        partial(gatekeeper_test, ROOT, scratch),
    )
    for case in cases:
        case()
    print("CI selector, event, test partition, check commands, rename/delete, gate and installer epoch self-tests passed")
