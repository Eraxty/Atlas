from config import load_config, save_config
from database import create_db
from groups_menu import groups_menu
from nzb import generate_nzb
from search import search_all_releases, search_releases
from pathlib import Path
import json
import os
import signal
import subprocess
import sys

BASE_DIR = Path(__file__).resolve().parent
PID_FILE = BASE_DIR / "bg_indexer.pid"
LOG_FILE = BASE_DIR / "bg_index.log"
STATUS_FILE = BASE_DIR / "status.json"

LOGO = r"""
        █████╗ ████████╗ ██╗      █████╗ ███████╗
        ██╔══██╗╚══██╔══╝██║     ██╔══██╗██╔════╝
        ███████║   ██║   ██║     ███████║███████╗
        ██╔══██║   ██║   ██║     ██╔══██║╚════██║
        ██║  ██║   ██║   ███████╗██║  ██║███████║
        ╚═╝  ╚═╝   ╚═╝   ╚══════╝╚═╝  ╚═╝╚══════╝
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


def read_int(prompt, default=None):
    while True:
        value = input(prompt).strip()

        if not value and default is not None:
            return default

        try:
            return int(value)
        except ValueError:
            print("Invalid input, please enter a number.\n")


def first_run_setup():
    print("No saved configuration.\n")

    host = input("Host: ")
    username = input("Username: ")
    password = input("Password: ")
    port = read_int("Port (563): ", 563)

    while True:
        group = input("\nNewsgroup (press Enter to browse, or enter one manually): ").strip()

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

    query = input("Search: ").strip()

    if scope == 1:
        releases = search_releases(query, config["group"])
    else:
        releases = search_all_releases(query)

    if not releases:
        print("\nNo releases found.")
        return

    print()
    for release in releases[:50]:
        print(f"[{release[0]}] {release[1]}")

    if len(releases) > 50:
        print(f"\n... and {len(releases) - 50} more")

    print("\n[0] Back")
    release_id = read_int("Release ID: ")

    if release_id == 0:
        return

    generate_nzb(release_id)


def settings_menu():
    clear()

    print("Settings")
    print("1. Change configuration")
    print("2. Back")

    choice = read_int("\nChoice: ")
    if choice != 1:
        return

    print()
    host = input("Host: ")
    username = input("Username: ")
    password = input("Password: ")
    group = input("Newsgroup: ")
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

        choice = input("\nChoice: ")

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
