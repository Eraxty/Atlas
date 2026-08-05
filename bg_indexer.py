from config import load_config
from nntp_client import NNTPClient
from indexer import Indexer
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

update_status(True, current_group)

try:
    while True:
        config = load_config()
        group = config["group"]

        if group != current_group:
            update_status(True, group)
            current_group = group

        try:
            indexer.index_group(group)
            time.sleep(0.1)

        except Exception as e:
            print(f"Indexing error ({group}): {e}")
            time.sleep(2)

finally:
    update_status(False, "")
    client.disconnect()