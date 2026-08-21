from src.config import load_config
from src.database import create_db
from src.nntp_client import NNTPClient
from src.indexer import Indexer
from src.colors import red, yellow, reset
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

if not config.get("password"):
    print(f"{yellow}no password stored in keyring, run main.py to set it up{reset}")
    sys.exit(1)

create_db()

client = NNTPClient(
    host = config["host"],
    username = config["username"],
    password = config["password"],
    port = config["port"],
)

indexer = Indexer(client, mode = config.get("index_mode", "dynamic"))

#main reads this file to draw the status line
def update_status(running, group, idle = False, status = "running", error = False, errors = 0):
    try:
        tmp = STATUS_FILE.with_suffix(".json.tmp")

        with open(tmp, "w") as f:
            json.dump({
                "running": running,
                "group": group,
                "error": error,
                "idle": idle,
                "mode": indexer.mode,
                "status": status,
                "error_count": errors,
                #soo main can tell if this process is actually alive
                "pid": os.getpid()
            }, f)

        os.replace(tmp, STATUS_FILE)

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
        time.sleep(max(0, min(1, deadline - time.time())))

current_group = config["group"]
errors = 0
error = False

#only rewrite status when the idle flag flips
last_written_idle = indexer.idle

#soo we can spot when the config changes
last_conn = (config["host"], config["username"], config["password"], config["port"])

update_status(True, current_group, indexer.idle, "running", error = error, errors = errors)

try:
    while not stop_requested:
        config = load_config()

        if config is None:
            print(f"{red}error with config, stopped{reset}")
            break

        conn = (config.get("host"), config.get("username"), config.get("password"), config.get("port"))

        if not all(conn):
            print(f"{red}config missing required fields{reset}")
            break

        if conn != last_conn:
            try:
                client.disconnect()
            except Exception:
                pass

            client.update_credentials(*conn)
            last_conn = conn
            print("config changed")

        group = config.get("group", "")
        mode = config.get("index_mode", "dynamic")

        if mode != indexer.mode:
            indexer.mode = mode
            indexer.phase = "backfill"
            indexer.idle = False
            indexer.backfilling = False
            last_written_idle = indexer.idle
            update_status(True, current_group, indexer.idle, "running", error = error, errors = errors)

        #new group picked soo reset the error count
        if group != current_group:
            current_group = group
            errors = 0
            indexer.phase = "backfill"
            indexer.idle = False
            indexer.backfilling = False
            last_written_idle = indexer.idle
            update_status(True, group, indexer.idle, "running", error = error, errors = errors)

        try:
            #reconnect on demand
            if not client.server:
                client.connect()

            #no group set in config yet
            if not group:
                print(f"{yellow}no newsgroup configured{reset}")
                break

            indexer.index_group(group)
            recovered = errors > 0
            errors = 0

            if indexer.idle != last_written_idle or recovered:
                update_status(True, current_group, indexer.idle, "idle" if indexer.idle else "running", error = error, errors = errors)
                last_written_idle = indexer.idle

            if indexer.idle and not indexer.backfilling:
                idle_sleep(10)
            else:
                time.sleep(0.1)

        except Exception as e:
            if stop_requested:
                break

            errors += 1

            #set warning status while we still have retries left
            if errors < 3:
                update_status(True, current_group, indexer.idle, "warning", error = error, errors = errors)

            #3 strikes and we stop bcs we aint hammering a dead server
            if errors >= 3:
                print(f"{red}Too many errors on {group} Stopping{reset}")
                error = True
                update_status(True, current_group, indexer.idle, "error", error = True, errors = errors)
                break

            print(f"{red}Indexing error ({group}): {e}{reset}")
            
            try:
                client.disconnect()
            except Exception:
                pass

            #back off before retrying
            time.sleep(min(2 ** errors, 30))

            try:
                client.connect()
            except Exception as reconnect_error:
                print(f"{red}Reconnect failed: {reconnect_error}{reset}")
except Exception as e:
    print(f"{red}indexer crashed: {e}{reset}")
    error = True
finally:
    update_status(False, "", status = "error" if error else "stopped", error = error, errors = errors)

    try:
        #only clear the pid if its ours, another one might be running
        if PID_FILE.read_text().strip() == str(os.getpid()):
            PID_FILE.unlink(missing_ok = True)

    except (OSError, ValueError):
        pass

    try:
        client.disconnect()
    except Exception:
        pass
