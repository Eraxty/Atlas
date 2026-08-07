from src.config import load_config
from src.nntp_client import NNTPClient
from src.indexer import Indexer
from pathlib import Path
import signal
import sys
import time
import json

BASE_DIR = Path(__file__).resolve().parent
STATUS_FILE = BASE_DIR / "status.json"

config = load_config()

client = NNTPClient(
    host=config["host"],
    username=config["username"],
    password=config["password"],
    port=config["port"],
)

client.connect()

indexer = Indexer(client)

def update_status(running, group):
    with open(STATUS_FILE, "w") as f:
        json.dump({
            "running": running,
            "group": group
        }, f)

def handle_stop(signum, frame):
    update_status(False, "")
    sys.exit(0)

signal.signal(signal.SIGTERM, handle_stop)

current_group = config["group"]
errors = 0

update_status(True, current_group)

try:
    while True:
        config = load_config()
        group = config["group"]

        if group != current_group:
            update_status(True, group)
            current_group = group
            errors = 0

        try:
            if not group:
                print("No newsgroup configured.")
                break

            indexer.index_group(group)
            errors = 0
            time.sleep(0.1)

        except Exception as e:
            errors += 1

            if errors >= 3:
                print(f"Too many errors on {group} Stopping")
                break

            print(f"Indexing error ({group}): {e}")
            time.sleep(2)

finally:
    update_status(False, "")
    client.disconnect()