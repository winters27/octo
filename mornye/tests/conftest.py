"""Lets the tests import the helper from the folder above, as the image runs it."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
