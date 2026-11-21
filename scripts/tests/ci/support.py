"""Shared filesystem root and assertion helpers for the CI tests."""

from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]


def expect_failure(check):
    try:
        check()
    except ValueError:
        return
    raise AssertionError("Expected rejection")
