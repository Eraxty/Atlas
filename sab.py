from pathlib import Path
import subprocess
import sys

BASE_DIR = Path(__file__).resolve().parent
SAB_DIR = BASE_DIR / "SABnzbd-5.0.4"

process = None


def start():
    global process

    if process and process.poll() is None:
        print("SAB is already running.")
        return

    process = subprocess.Popen(
        [sys.executable, "SABnzbd.py"],
        cwd=SAB_DIR,
    )

    print("Started SABnzbd")


def stop():
    global process

    if process and process.poll() is None:
        process.terminate()
        process.wait()
        print("Stopped")
    else:
        print("SAB isn't running bro")


def is_running():
    return process is not None and process.poll() is None