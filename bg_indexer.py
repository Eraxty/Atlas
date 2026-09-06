from src.config import load_config
from src.database import create_db
from src.nntp_client import NNTPClient
from src.indexer import Indexer
from src.colors import red, yellow, reset
from src.sab import start as start_sab, is_running as sab_running, wait_ready as sab_wait_ready
from pathlib import Path
import os
import signal
import sys
import time
import json

BASE_DIR = Path(__file__).resolve().parent
STATUS_FILE = BASE_DIR / "status.json"
STATS_FILE = BASE_DIR / "stats.json"
PID_FILE = BASE_DIR / "bg_indexer.pid"

HISTORY_LEN = 60


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


class Stats:
    def __init__(self):
        self.history = []
        self.total_articles = 0
        self.total_bytes = 0
        self.total_releases = 0
        self.start_time = time.time()
        self.groups_indexed = set()
        self.error_count = 0
        self.group_stats = {}

    def tick(self, articles, bytes_downloaded, releases = 0, group = None):
        now = time.time()
        self.total_articles += articles
        self.total_bytes += bytes_downloaded
        self.total_releases += releases
        self.history.append({"t": now, "a": articles, "b": bytes_downloaded})

        if group:
            self.groups_indexed.add(group)
            gs = self.group_stats.setdefault(group, {"articles": 0, "releases": 0})
            gs["articles"] += articles
            gs["releases"] += releases
            gs["last_indexed"] = now

        cutoff = now - HISTORY_LEN
        self.history = [h for h in self.history if h["t"] >= cutoff]

    def record_error(self):
        self.error_count += 1

    def _calc_speeds(self):
        if len(self.history) < 2:
            return 0, 0, 0, 0

        a_speeds = []
        b_speeds = []

        for i in range(1, len(self.history)):
            dt = self.history[i]["t"] - self.history[i - 1]["t"]
            if dt > 0:
                a_speeds.append(self.history[i]["a"] / dt)
                b_speeds.append(self.history[i]["b"] / dt)

        if not a_speeds:
            return 0, 0, 0, 0

        return (
            max(a_speeds),
            max(b_speeds),
            sum(a_speeds) / len(a_speeds),
            sum(b_speeds) / len(b_speeds),
        )

    def _db_size(self):
        db = BASE_DIR / "atlas.db"

        if db.exists():
            return db.stat().st_size

        return 0

    def write(self, group, mode, running, idle):
        peak_a, peak_b, avg_a, avg_b = self._calc_speeds()

        groups_data = {}

        for name, gs in self.group_stats.items():
            groups_data[name] = {
                "articles": gs["articles"],
                "releases": gs["releases"],
                "last_indexed": gs.get("last_indexed", 0),
            }

        try:
            data = {
                "running": running,
                "idle": idle,
                "group": group,
                "mode": mode,
                "uptime": int(time.time() - self.start_time),
                "total_articles": self.total_articles,
                "total_bytes": self.total_bytes,
                "total_releases": self.total_releases,
                "history": self.history[-HISTORY_LEN:],
                "groups_indexed": len(self.groups_indexed),
                "error_count": self.error_count,
                "peak_art_speed": peak_a,
                "peak_byte_speed": peak_b,
                "avg_art_speed": avg_a,
                "avg_byte_speed": avg_b,
                "db_size": self._db_size(),
                "groups": groups_data,
            }
            
            tmp = STATS_FILE.with_suffix(".json.tmp")

            with open(tmp, "w") as f:
                json.dump(data, f)
            
            os.replace(tmp, STATS_FILE)
        
        except OSError:
            pass


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

    #start sabnzbd so its ready when you wanna download
    if not sab_running():
        start_sab()
        sab_wait_ready()

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
    stats = Stats()

    last_written_idle = None
    last_stats_write = 0
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

                stats.tick(
                    indexer.last_batch_articles,
                    indexer.last_batch_bytes,
                    indexer.last_batch_releases,
                    group,
                )

                now = time.time()
                
                if now - last_stats_write >= 1:
                    idle_now = indexer.all_idle([g for g in groups if g not in failed]) and not any(indexer.is_backfilling(g) for g in groups if g not in failed)
                    stats.write(group, indexer.mode, True, idle_now)
                    last_stats_write = now

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

                stats.record_error()
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
        stats.write("", "", False, False)

        try:
            client.disconnect()
        except Exception:
            pass


if __name__ == "__main__":
    main()
