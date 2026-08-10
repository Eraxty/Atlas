from src.config import load_config
from src.nntp_client import NNTPClient
from src.indexer import Indexer
from pathlib import Path
import os
import signal
import time
import json

BASE_DIR = Path(__file__).resolve().parent
STATUS_FILE = BASE_DIR / "status.json"
PID_FILE = BASE_DIR / "bg_indexer.pid"

config = load_config()

client = NNTPClient(
    host=config["host"],
    username=config["username"],
    password=config["password"],
    port=config["port"],
)

indexer = Indexer(client, mode = config.get("index_mode", "dynamic"))

def update_status(running, group, idle=False):
    with open(STATUS_FILE, "w") as f:
        json.dump({
            "running": running,
            "group": group,
            "error": error,
            "idle": idle,
            "mode": indexer.mode,
            "pid": os.getpid()
        }, f)

stop_requested = False

def handle_stop(signum, frame):
    global stop_requested
    stop_requested = True

signal.signal(signal.SIGTERM, handle_stop)

def idle_sleep(duration):
    deadline = time.time() + duration
    while not stop_requested and time.time() < deadline:
        time.sleep(min(1, deadline - time.time()))

current_group = config["group"]
errors = 0
error = False
last_written_idle = indexer.idle

update_status(True, current_group, indexer.idle)

try:
    while not stop_requested:
        config = load_config()
        group = config["group"]
        indexer.mode = config.get("index_mode", "dynamic")

        if group != current_group:
            update_status(True, group, indexer.idle)
            current_group = group
            errors = 0
            last_written_idle = indexer.idle

        try:
            if not client.server:
                client.connect()

            if not group:
                print("no newsgroup configured")
                break

            indexer.index_group(group)
            errors = 0

            if indexer.idle != last_written_idle:
                update_status(True, current_group, indexer.idle)
                last_written_idle = indexer.idle

            if indexer.idle and not indexer.backfilling:
                idle_sleep(10)
            else:
                time.sleep(0.1)

        except Exception as e:
            if stop_requested:
                break

            errors += 1

            if errors >= 3:
                print(f"Too many errors on {group} Stopping")
                error = True
                break

            print(f"Indexing error ({group}): {e}")
            
            try:
                client.disconnect()
            except Exception:
                pass

            time.sleep(2)

            try:
                client.connect()
            except Exception as reconnect_error:
                print(f"Reconnect failed: {reconnect_error}")
finally:
    update_status(False, "")

    try:
        if PID_FILE.read_text().strip() == str(os.getpid()):
            PID_FILE.unlink(missing_ok=True)

    except (OSError, ValueError):
        pass

    try:
        client.disconnect()
    except Exception:
        pass