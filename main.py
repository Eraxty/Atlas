from src.config import load_config, save_config
from src.database import create_db
from src.download import download_release
from src.groups_menu import groups_menu
from src.nzb import generate_nzb
from src.prompts import prompt
from src.search import count_all_releases, count_releases, get_release, search_all_releases, search_releases
from pathlib import Path
import json
import math
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time

#paths
BASE_DIR = Path(__file__).resolve().parent
PID_FILE = BASE_DIR / "bg_indexer.pid"
LOG_FILE = BASE_DIR / "bg_index.log"
STATUS_FILE = BASE_DIR / "status.json"

#my logo
LOGO = r"""
         █████╗ ████████╗ ██╗       █████╗  ███████╗
        ██╔══██╗╚══██╔══╝ ██║      ██╔══██╗ ██╔════╝
        ███████║   ██║    ██║      ███████║ ███████╗
        ██╔══██║   ██║    ██║      ██╔══██║ ╚════██║
        ██║  ██║   ██║    ███████╗ ██║  ██║ ███████║
        ╚═╝  ╚═╝   ╚═╝    ╚══════╝ ╚═╝  ╚═╝ ╚══════╝
"""


def clear():
    os.system("cls" if os.name == "nt" else "clear")


def fmt_size(size):
    if not size:
        return "0 B"

    #try the units till it fits
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if size < 1024:
            return f"{size:.1f} {unit}"
        size /= 1024

    return f"{size:.1f} PB"


def fmt_date(value):
    if not value:
        return ""

    return str(value)[:10]


def get_status():
    try:
        with open(STATUS_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {"running": False, "group": ""}


def indexer_alive():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
    except ValueError:
        PID_FILE.unlink(missing_ok=True)
        return False

    try:
        status = get_status()
    except Exception:
        status = {}

    if status.get("pid") != pid:
        return False

    try:
        #signal 0 checks if alive
        os.kill(pid, 0)

        #pids get reused soo double check its our indexer
        if Path("/proc").exists():
            try:
                cmdline = Path(f"/proc/{pid}/cmdline").read_bytes()
            except OSError:
                cmdline = b""

            if b"bg_indexer.py" not in cmdline:
                PID_FILE.unlink(missing_ok=True)
                return False

        return True

    except ProcessLookupError:
        PID_FILE.unlink(missing_ok=True)
        return False

    except PermissionError:
        return True


def start_background_indexer():
    if indexer_alive():
        return False

    try:
        with LOG_FILE.open("a") as log_file:
            process = subprocess.Popen(
                [sys.executable, "-u", "bg_indexer.py"],
                cwd = BASE_DIR,
                stdin = subprocess.DEVNULL,
                stdout = log_file,
                stderr = subprocess.STDOUT,
                start_new_session = True,
            )
   
    except OSError as e:
        print(f"couldnt start indexer: {e}")
        return False

    PID_FILE.write_text(str(process.pid))
    return True


def stop_background_indexer():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
    except ValueError:
        PID_FILE.unlink(missing_ok=True)
        return False

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        PID_FILE.unlink(missing_ok=True)
        return False

    #5 sec to comply or die
    for _ in range(50):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            PID_FILE.unlink(missing_ok=True)
            return True

        time.sleep(0.1)

    #didnt exit in time soo kill him
    try:
        os.kill(pid, signal.SIGKILL)
    
    except ProcessLookupError:
        PID_FILE.unlink(missing_ok = True)
        return True

    PID_FILE.unlink(missing_ok = True)
    return True
    

def ask(text, default=None):
    while True:
        value = prompt(text).strip()

        if not value and default is not None:
            return default

        try:
            return int(value)
        except ValueError:
            print("that aint a number\n")


def setup():
    print("no config found")

    host = prompt("Host: ")
    username = prompt("Username: ")
    password = prompt("Password: ")
    port = ask("Port (563): ", 563)

    save_config(host, username, password, port, "")
    print("\ngroup empty rn, select one from the Groups menu")


def do_search(config):
    while True:
        clear()

        print("Search")
        print("1. Current Group")
        print("2. All Groups")
        print("0. Back")

        scope = ask("\nChoice: ")
        if scope not in (1, 2):
            return

        query = prompt("Search: ").strip()

        if not query or query == "0":
            continue

        page = 0

        # terminal size
        page_size = max(10, shutil.get_terminal_size().lines - 15)

        while True:
            try:
                if scope == 1:
                    total = count_releases(query, config["group"])
                    releases = search_releases(query, config["group"], page, page_size)
                
                else:
                    total = count_all_releases(query)
                    releases = search_all_releases(query, page, page_size)
            
            except sqlite3.Error:
                print("\ncouldnt search, db error")
                return

            if not total:
                print("\nno releases found")
                query = prompt("\nSearch: ").strip()
                if not query or query == "0":
                    break
                page = 0
                continue

            total_pages = max(1, math.ceil(total / page_size))

            if page > total_pages - 1:
                page = total_pages - 1
                continue

            #only ids on this page are valid
            release_ids = {release[0] for release in releases}

            clear()

            print(f"Search: {query}")
            print(f"Page {page + 1} of {total_pages}")

            start = page * page_size + 1
            end = min((page + 1) * page_size, total)
            print(f"Showing {start}-{end} of {total} results\n")

            for release in releases:
                name = release[1]

                if len(name) > 42:
                    name = name[:39] + "..."

                broken = "  [broken]" if not release[6] else ""

                print(f"[{release[0]}] {name}  {fmt_size(release[5])} - {release[7]} parts - {fmt_date(release[4])}{broken}")

            print("\n0. Back")

            if page > 0:
                print("p. Previous Page")
            if page < total_pages - 1:
                print("n. Next Page")

            print("g. Go to Page")

            choice = prompt("\nChoice: ").strip()

            if choice == "0":
                return

            if choice == "p":
                if page > 0:
                    page -= 1
                else:
                    print("already on the first page")
                    prompt("[enter]")
                continue

            if choice == "n":
                if page < total_pages - 1:
                    page += 1
                else:
                    print("already on the last page")
                    prompt("[enter]")
                continue

            if choice == "g":
                goto = prompt(f"Go to page (1-{total_pages}): ")

                try:
                    target = int(goto)
                except ValueError:
                    target = -1

                if 1 <= target <= total_pages:
                    page = target - 1
                else:
                    print(f"page must be between 1 and {total_pages}")
                    prompt("[enter]")
                continue

            try:
                choice_id = int(choice)
            except ValueError:
                print("invalid")
                prompt("[enter]")
                continue

            if choice_id not in release_ids:
                print("not valid release id")
                prompt("[enter]")
                continue

            release = get_release(choice_id)
            release_name = release[1] if release else f"#{choice_id}"

            while True:
                clear()

                #actions for the picked release
                print(f"Release: {release_name}")

                if release:
                    print(f"{fmt_size(release[5])} - {release[7]} parts - {fmt_date(release[4])}")

                print("1. Download")
                print("2. Save NZB")
                print("0. Back")

                choice = prompt("\nChoice: ").strip()

                if choice == "1":
                    try:
                        ok = download_release(choice_id)
                    
                    except Exception as e:
                        print(f"couldnt queue download: {e}")
                        ok = False

                    if ok:
                        print("\nDownload queued it is downloading in background.")
                        print("Finished files land ~/Downloads")
                    prompt("[enter]")
                    return

                if choice == "2":
                    try:
                        generate_nzb(choice_id)
                    except Exception as e:
                        print(f"couldnt save nzb: {e}")
                    prompt("[enter]")
                    return

                if choice == "0":
                    break

                print("invalid")
                prompt("[enter]")

def do_settings():
    clear()

    config = load_config()

    print("Settings")
    print(f"Indexer mode : {config.get('index_mode', 'dynamic')}")
    print("1. Change config")
    print("2. Change indexer mode")
    print("0. Back")

    choice = ask("\nChoice: ")

    if choice == 2:
        while True:
            clear()

            print("Indexer mode")
            print(f"Current : {config.get('index_mode', 'dynamic')}")
            print("1. dynamic")
            print("2. live")
            print("3. backfill")
            print("0. Back")

            mode = ask("\nChoice: ")

            if mode == 0:
                return

            #modes
            modes = {1: "dynamic", 2: "live", 3: "backfill"}

            if mode not in modes:
                print("that is not a number")
                continue

            save_config(
                config["host"],
                config["username"],
                config["password"],
                config["port"],
                config["group"],
                modes[mode],
            )

            print(f"indexer mode set to {modes[mode]}")
            return

    if choice != 1:
        return

    print()
    host = prompt(f"Host ({config['host']}): ").strip() or config["host"]
    username = prompt(f"Username ({config['username']}): ").strip() or config["username"]
    password = prompt("Password: ").strip() or config["password"]

    while True:
        group = prompt("Newsgroup: ").strip()
        if group:
            break
        print("newsgroup cant be empty\n")

    port = ask("Port (563): ", 563)

    save_config(host, username, password, port, group, config.get("index_mode", "dynamic"))
    
    print("\nsaved")


def main():
    create_db()

    config = load_config()

    if config:
        print("config loaded:")
        print(f"Server: {config['host']}")
        print(f"Current Group: {config['group']}\n")
    else:
        setup()
        config = load_config()

    while True:
        clear()

        indexing = indexer_alive()
        status = get_status()

        print("=" * 55)
        print(LOGO)
        print("=" * 55)
        print(f"Current Group : {config['group']}")

        #build the status line
        if indexing:
            label = status["group"] or config["group"]
            
            if status.get("mode"):
                label += f" [{status['mode']}]"
            
            if status.get("idle"):
                label += " (idle)"
            
            print(f"Indexing      : {label}")
        
        elif status.get("error"):
            print("Indexing      : stopped (error)")
        
        else:
            print("Indexing      : stopped")

        print("=" * 55)

        if indexing:
            print("1. Stop Indexing")
        else:
            print("1. Start Indexing")

        print("2. Search")
        print("3. Groups")
        print("4. Settings")
        print("0. Exit")
        print("=" * 55)

        choice = prompt("\nChoice: ")

        if choice == "1":
            if indexing:
                stopped = stop_background_indexer()
                print("indexing stopped" if stopped else "indexer wasnt running")
            else:
                started = start_background_indexer()
                print("indexing started" if started else "indexer already running")

        elif choice == "2":
            do_search(config)

        elif choice == "3":
            groups_menu(config)
            config = load_config()

        elif choice == "4":
            do_settings()
            #reload so the new group shows up
            config = load_config()

        elif choice == "0":
            #byee
            print("\nbyee.")
            break


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        stop_background_indexer()
        #byee
        print("\nbyeee")
