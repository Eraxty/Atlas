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


def update_status(running, group, indexer, idle = False, status = "running", error = False, errors = 0):
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
                "pid": os.getpid()
            }, f)

        os.replace(tmp, STATUS_FILE)

    except OSError as e:
        print(f"couldnt write status: {e}")


def idle_sleep(duration, is_stopped):
    deadline = time.time() + duration
    while not is_stopped() and time.time() < deadline:
        time.sleep(max(0, min(1, deadline - time.time())))


def tracked_groups(cfg):
    groups = [g for g in (cfg.get("groups") or []) if g]

    if not groups and cfg.get("group"):
        groups = [cfg["group"]]

    return groups


def main():
    config = load_config()

    if not config or not config.get("host"):
        print("no valid config")
        sys.exit(1)

    if not config.get("password"):
        print(f"{yellow}no password stored in keyring, run main.py to set it up{reset}")
        sys.exit(1)

    tmp = PID_FILE.with_suffix(".pid.tmp")
    tmp.write_text(str(os.getpid()))
    os.replace(tmp, PID_FILE)

    create_db()

    client = NNTPClient(
        host = config["host"],
        username = config["username"],
        password = config["password"],
        port = config["port"],
    )

    indexer = Indexer(client, mode = config.get("index_mode", "dynamic"))

    stop_requested = False

    def handle_stop(signum, frame):
        nonlocal stop_requested
        stop_requested = True

    signal.signal(signal.SIGTERM, handle_stop)

    groups = tracked_groups(config)
    group_idx = 0
    errors = {}
    failed = set()
    error = False

    last_written_idle = None
    last_conn = (config["host"], config["username"], config["password"], config["port"])

    update_status(True, ", ".join(groups), indexer, indexer.all_idle(groups), "running", error=error)

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

            groups = tracked_groups(config)
            failed &= set(groups)

            for g in list(failed):
                if g not in groups:
                    failed.discard(g)

            if not groups:
                idle_sleep(10, lambda: stop_requested)
                continue

            mode = config.get("index_mode", "dynamic")

            if mode != indexer.mode:
                indexer.mode = mode

                for st in indexer.state.values():
                    st.update(phase ="backfill", idle = False, backfilling = False)

                last_written_idle = None

            group = groups[group_idx % len(groups)]
            group_idx += 1

            if group in failed:
                continue

            try:
                if not client.server:
                    client.connect()

                indexer.index_group(group)

                recovered = errors.get(group, 0) > 0
                errors[group] = 0
                failed.discard(group)

                active = [g for g in groups if g not in failed]
                idle_now = indexer.all_idle(active) and not any(indexer.is_backfilling(g) for g in active)

                if idle_now != last_written_idle or recovered:
                    update_status(True, ", ".join(groups), indexer, idle_now, "idle" if idle_now else "running", error=error, errors=sum(errors.values()))
                    last_written_idle = idle_now

                if idle_now:
                    idle_sleep(10, lambda: stop_requested)
                else:
                    time.sleep(0.1)

            except Exception as e:
                if stop_requested:
                    break

                errors[group] = errors.get(group, 0) + 1

                if errors[group] < 3:
                    update_status(True, ", ".join(groups), indexer, indexer.is_idle(group), "warning", error=error, errors=sum(errors.values()))

                if errors[group] >= 3:
                    print(f"{red}Too many errors on {group}, skipping it{reset}")
                    failed.add(group)
                    continue

                print(f"{red}Indexing error ({group}): {e}{reset}")

                try:
                    client.disconnect()
                except Exception:
                    pass

                time.sleep(min(2 ** errors[group], 30))

                try:
                    client.connect()
                except Exception as reconnect_error:
                    print(f"{red}Reconnect failed: {reconnect_error}{reset}")

    except Exception as e:
        print(f"{red}indexer crashed: {e}{reset}")
        error = True

    finally:
        update_status(False, "", indexer, status="error" if error else "stopped", error=error, errors=sum(errors.values()))

        try:
            if PID_FILE.read_text().strip() == str(os.getpid()):
                PID_FILE.unlink(missing_ok = True)

        except (OSError, ValueError):
            pass

        try:
            client.disconnect()
        except Exception:
            pass


if __name__ == "__main__":
    main()
