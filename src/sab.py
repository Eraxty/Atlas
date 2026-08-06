from pathlib import Path
import configparser
import subprocess
import sys
import time
import urllib.request

BASE_DIR = Path(__file__).resolve().parent.parent
SAB_DIR = BASE_DIR / "SABnzbd-5.0.4"
LOG_FILE = BASE_DIR / "sabnzbd.log"

CONFIG_DIR = Path.home() / ".sabnzbd"
CONFIG_FILE = CONFIG_DIR / "sabnzbd.ini"
WATCHED_DIR = CONFIG_DIR / "watched"

process = None


def start():
    global process

    if process and process.poll() is None:
        print("SAB is already running.")
        return True

    configure_watched_dir()

    with LOG_FILE.open("a") as log_file:
        process = subprocess.Popen(
            [sys.executable, "-u", "SABnzbd.py"],
            cwd = SAB_DIR,
            stdin = subprocess.DEVNULL,
            stdout = log_file,
            stderr = subprocess.STDOUT,
            start_new_session = True,
        )

    print("Started SABnzbd.")
    return True


def stop():
    global process

    if process and process.poll() is None:
        process.terminate()
        process.wait()
        print("Stopped")
    else:
        print("SAB isn't running bro")


def is_running():
    global process

    if process and process.poll() is None:
        return True

    try:
        with urllib.request.urlopen(get_url(), timeout=2):
            return True

    except OSError:
        return False


def load_config():
    config = configparser.ConfigParser()

    if CONFIG_FILE.exists():
        with open(CONFIG_FILE, encoding="utf-8") as f:
            text = f.read()

        if text.startswith("__version__"):
            text = "[DEFAULT]\n" + text

        config.read_string(text)

    if "misc" not in config:
        config["misc"] = {}

    return config


def save_config(config):
    CONFIG_DIR.mkdir(parents=True, exist_ok=True)

    with open(CONFIG_FILE, "w") as f:
        config.write(f)


def get_watched_dir():
    config = load_config()

    folder = config["misc"].get("dirscan_dir")

    if folder:
        folder = folder.strip().strip('"').strip()

    if not folder:
        return None

    path = Path(folder)

    if not path.is_absolute():
        path = CONFIG_DIR / path

    return path


def configure_watched_dir():
    folder = get_watched_dir()

    if folder is None:
        folder = WATCHED_DIR

        config = load_config()
        config["misc"]["dirscan_dir"] = str(folder)
        save_config(config)

    folder.mkdir(parents=True, exist_ok=True)

    return folder


def get_url():
    config = load_config()

    host = config["misc"].get("host", "127.0.0.1")
    port = config["misc"].get("port", "8080")

    return f"http://{host}:{port}/"


def wait_ready(timeout=60):
    global process

    deadline = time.time() + timeout

    while time.time() < deadline:

        try:
            with urllib.request.urlopen(get_url(), timeout=2):
                return True

        except OSError:
            pass

        if process and process.poll() is not None:
            return False

        time.sleep(1)

    return False