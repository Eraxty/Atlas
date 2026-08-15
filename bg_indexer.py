from src.config import load_config
from src.nntp_client import NNTPClient
from src.indexer import Indexer
from pathlib import Path
import os
import signal
import sys
import time
import json

BASE_DIR = Path(__file__).resolve().parent
STATUS_FILE = BASE_DIR / "status.json"
PID_FILE = BASE_DIR / "bg_indexer.pid"

config = load_config()

if not config or not config.get("host"):
    print("no valid config")
    sys.exit(1)

client = NNTPClient(
    host=config["host"],
    username=config["username"],
    password=config["password"],
    port=config["port"],
)

indexer = Indexer(client, mode = config.get("index_mode", "dynamic"))

#main reads this file to draw the status line
def update_status(running, group, idle=False):
    try:
        with open(STATUS_FILE, "w") as f:
            json.dump({
                "running": running,
                "group": group,
                "error": error,
                "idle": idle,
                "mode": indexer.mode,
                #soo main can tell if this process is actually alive
                "pid": os.getpid()
            }, f)
    
    except OSError as e:
        print(f"couldnt write status: {e}")

stop_requested = False

#flip the flag soo the loop exits clean on SIGTERM
def handle_stop(signum, frame):
    global stop_requested
    stop_requested = True

signal.signal(signal.SIGTERM, handle_stop)

#sleep in small chunks soo a stop request gets out fast
def idle_sleep(duration):
    deadline = time.time() + duration
    while not stop_requested and time.time() < deadline:
        time.sleep(min(1, deadline - time.time()))

current_group = config["group"]
errors = 0
error = False
#only rewrite status when the idle flag flips
last_written_idle = indexer.idle
#soo we can spot when the config changes
last_conn = (config["host"], config["username"], config["password"], config["port"])

update_status(True, current_group, indexer.idle)

try:
    while not stop_requested:
        config = load_config()

        if config is None:
            print("error with config, stopped")
            break

        conn = (config["host"], config["username"], config["password"], config["port"])

        if conn != last_conn:
            try:
                client.disconnect()
            except Exception:
                pass

            client.update_credentials(*conn)
            last_conn = conn
            print("config changed")

        group = config["group"]
        indexer.mode = config.get("index_mode", "dynamic")

        #new group picked soo reset the error count
        if group != current_group:
            update_status(True, group, indexer.idle)
            current_group = group
            errors = 0
            last_written_idle = indexer.idle

        try:
            #reconnect on demand
            if not client.server:
                client.connect()

            #no group set in config yet
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

            #3 strikes and we stop bcs we aint hammering a dead server
            if errors >= 3:
                print(f"Too many errors on {group} Stopping")
                error = True
                break

            print(f"Indexing error ({group}): {e}")
            
            try:
                client.disconnect()
            except Exception:
                pass

            #back off before retrying
            time.sleep(2)

            try:
                client.connect()
            except Exception as reconnect_error:
                print(f"Reconnect failed: {reconnect_error}")
finally:
    update_status(False, "")

    try:
        #only clear the pid if its ours, another one might be running
        if PID_FILE.read_text().strip() == str(os.getpid()):
            PID_FILE.unlink(missing_ok=True)

    except (OSError, ValueError):
        pass

    try:
        client.disconnect()
    except Exception:
        pass