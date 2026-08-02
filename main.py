from database import create_db
from config import load_config, save_config
from search import search_releases
from nzb import generate_nzb
import os
import subprocess
import sys
from pathlib import Path
import signal
from groups_menu import groups_menu
import json

BASE_DIR = Path(__file__).resolve().parent
PID_FILE = BASE_DIR / "bg_indexer.pid"
LOG_FILE = BASE_DIR / "bg_index.log"

def get_status():
    try:
        with open("status.json") as f:
            return json.load(f)
    except:
        return {
            "running":False,
            "group":""
        }

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


create_db()

config = load_config()

if config:
    print("Loaded config:")
    print(f"Server: {config['host']}")
    print(f"Current Group: {config['group']}\n")

else:
    print("No saved configuration.\n")

    host = input("Host: ")
    username = input("Username: ")
    password = input("Password: ")
    group = input("Newsgroup: ")
    port = int(input("Port (563): ") or "563")

    save_config(host, username, password, port, group)
    config = load_config()


while True:

    indexing = worker_is_running()

    status = get_status()

    print("Atlas\n")
    print(f"Current Group : {config["group"]}")

    if status["running"]:
        print(f"Indexing: {status["group"]}")
    else:
        print("Indexing: stopped")

    print()

    if indexing:
        choice = input(
            "1. Stop Indexing\n"
            "2. Search\n"
            "3. Groups\n"
            "4. Settings\n"
            "Choice: "
        )

    else:
        choice = input(
            "1. Start Indexing\n"
            "2. Search\n"
            "3. Groups\n"
            "4. Settings\n"
            "Choice: "
        )

    if choice == "1":

        if indexing:

            stopped = stop_background_indexer()

            if stopped:
                print("Background indexing stopped.")
            else:
                print("Background indexer is not running.")

        else:

            started = start_background_indexer()

            if started:
                print("Background indexing started.")
            else:
                print("Background indexing is already running.")

    elif choice == "2":

        query = input("Search: ")

        releases = search_releases(query)

        if not releases:
            print("No releases found.")

        else:
            for release in releases:
                print(f"[{release[0]}] {release[1]}")

            print("[0] Back")

            release_id = int(input("Release ID: "))

            if release_id == 0:
                continue

            generate_nzb(release_id)

    elif choice == "3":

        groups_menu(config)
        config = load_config()

    elif choice == "4":

        while True:

            print("Settings")
            print("1. Change configuration")
            print("2. Back")

            settings = input("Choice: ")

            if settings == "1":

                host = input("Host: ")
                username = input("Username: ")
                password = input("Password: ")
                group = input("Newsgroup: ")
                port = int(input("Port (563): ") or "563")

                save_config(host, username, password, port, group)
                config = load_config()

                print("Saved.")

            elif settings == "2":
                break