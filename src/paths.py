from pathlib import Path
import os
import sys


def app_dir():
    if getattr(sys, "frozen", False):
        base = Path(sys.executable).resolve().parent
    else:
        base = Path(__file__).resolve().parent.parent

    return Path(os.environ.get("ATLAS_HOME", base))