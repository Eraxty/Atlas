
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
import subprocess
import sys

BASE_DIR = Path(__file__).resolve().parent
PID_FILE = BASE_DIR / "bg_indexer.pid"
LOG_FILE = BASE_DIR / "bg_index.log"
STATUS_FILE = BASE_DIR / "status.json"

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


def get_status():
    try:
        with open(STATUS_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {"running": False, "group": ""}


def worker_is_running():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
        os.kill(pid, 0)
        return True

    except (ValueError, ProcessLookupError):
        PID_FILE.unlink(missing_ok=True)
        return False

    except PermissionError:
        return True


def start_background_indexer():
    if worker_is_running():
        return False

    with LOG_FILE.open("a") as log_file:
        process = subprocess.Popen(
            [sys.executable, "-u", "bg_indexer.py"],
            cwd=BASE_DIR,
            stdin=subprocess.DEVNULL,
            stdout=log_file,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    PID_FILE.write_text(str(process.pid))
    return True


def stop_background_indexer():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
        os.kill(pid, signal.SIGTERM)
        PID_FILE.unlink(missing_ok=True)
        return True

    except (ValueError, ProcessLookupError):
        PID_FILE.unlink(missing_ok=True)
        return False


def read_int(prompt_text, default=None):
    while True:
        value = prompt(prompt_text).strip()

        if not value and default is not None:
            return default

        try:
            return int(value)
        except ValueError:
            print("Invalid input, please enter a number.\n")


def first_run_setup():
    print("No saved configuration.\n")

    host = prompt("Host: ")
    username = prompt("Username: ")
    password = prompt("Password: ")
    port = read_int("Port (563): ", 563)

    while True:
        group = prompt("\nNewsgroup (press Enter to browse, or enter one manually): ").strip()

        if not group:
            save_config(host, username, password, port, group)
            print("\nChoose a newsgroup.\n")
            groups_menu(load_config())
            return

        if group.count(".") >= 2:
            save_config(host, username, password, port, group)
            return

        print("Newsgroup not found.")


def search_menu(config):
    clear()

    print("Search")
    print("1. Current Group")
    print("2. All Groups")
    print("0. Back")

    scope = read_int("\nChoice: ")
    if scope not in (1, 2):
        return

    query = prompt("Search: ").strip()

    page = 0
    page_size = max(10, shutil.get_terminal_size().lines - 15)

    while True:
        if scope == 1:
            total = count_releases(query, config["group"])
            releases = search_releases(query, config["group"], page, page_size)
        else:
            total = count_all_releases(query)
            releases = search_all_releases(query, page, page_size)

        if not total:
            print("\nNo releases found.")
            return

        total_pages = max(1, math.ceil(total / page_size))

        if page > total_pages - 1:
            page = total_pages - 1
            continue

        release_ids = {release[0] for release in releases}

        clear()

        print(f"Search: {query}")
        print(f"Page {page + 1} of {total_pages}")

        start = page * page_size + 1
        end = min((page + 1) * page_size, total)
        print(f"Showing {start}-{end} of {total} results\n")

        for release in releases:
            print(f"[{release[0]}] {release[1]}")

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
                print("Already on the first page.")
                prompt("Press Enter to continue...")
            continue

        if choice == "n":
            if page < total_pages - 1:
                page += 1
            else:
                print("Already on the last page.")
                prompt("Press Enter to continue...")
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
                print(f"Page must be between 1 and {total_pages}.")
                prompt("Press Enter to continue...")
            continue

        try:
            choice_id = int(choice)
        except ValueError:
            print("Invalid choice.")
            prompt("Press Enter to continue...")
            continue

        if choice_id not in release_ids:
            print("Invalid release ID.")
            prompt("Press Enter to continue...")
            continue

        release = get_release(choice_id)
        release_name = release[1] if release else f"#{choice_id}"

        while True:
            clear()

            print(f"Release: {release_name}")
            print("1. Download")
            print("2. Save NZB")
            print("0. Back")

            choice = prompt("\nChoice: ").strip()

            if choice == "1":
                download_release(choice_id)
                return

            if choice == "2":
                generate_nzb(choice_id)
                return

            if choice == "0":
                break

            print("Invalid choice.")
            prompt("Press Enter to continue...")

def settings_menu():
    clear()

    print("Settings")
    print("1. Change configuration")
    print("2. Back")

    choice = read_int("\nChoice: ")
    if choice != 1:
        return

    print()
    host = prompt("Host: ")
    username = prompt("Username: ")
    password = prompt("Password: ")
    group = prompt("Newsgroup: ")
    port = read_int("Port (563): ", 563)

    save_config(host, username, password, port, group)
    print("\nSaved.")


def main():
    create_db()

    config = load_config()

    if config:
        print("Loaded config:")
        print(f"Server: {config['host']}")
        print(f"Current Group: {config['group']}\n")
    else:
        first_run_setup()
        config = load_config()

    while True:
        clear()

        indexing = worker_is_running()
        status = get_status()

        print("=" * 55)
        print(LOGO)
        print("=" * 55)
        print(f"Current Group : {config['group']}")

        if indexing:
            print(f"Indexing      : {status['group'] or config['group']}")
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
                print("Background indexing stopped." if stopped else "Background indexer is not running.")
            else:
                started = start_background_indexer()
                print("Background indexing started." if started else "Background indexing is already running.")

        elif choice == "2":
            search_menu(config)

        elif choice == "3":
            groups_menu(config)
            config = load_config()

        elif choice == "4":
            settings_menu()
            config = load_config()

        elif choice == "0":
            print("\nbyee.")
            break


if __name__ == "__main__":
    main()
