"""Release CLI self-test runner."""
import sys
sys.dont_write_bytecode = True
from tests.release.test_format_archive import self_test
