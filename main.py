from database import create_db
from config import load_config, save_config
from search import search_releases
from nzb import generate_nzb
import os
import subprocess
import sys
from pathlib import Path
import signal
from nntp_client import NNTPClient
from groups import get_categories

BASE_DIR = Path(__file__).resolve().parent
PID_FILE = BASE_DIR / "bg_indexer.pid"
LOG_FILE = BASE_DIR / "bg_index.log"


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
            [sys.executable,"-u", "bg_indexer.py"],
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
    print(f"Newsgroup: {config['group']}\n")

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

    if indexing:
        choice = input(
            "1. Stop Indexing\n"
            "2. Search\n"
            "3. Settings\n"
            "Choice: "
        )

    else:
        choice = input(
            "1. Start Indexing\n"
            "2. Search\n"
            "3. Settings\n"
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

            release_id = int(input("Release ID: "))
            generate_nzb(release_id)

    elif choice == "3":

        client = NNTPClient(
            config["host"],
            config["username"],
            config["password"],
            config["port"]
        )

        client.connect()

        categories = get_categories(client)
        print("\nCategories\n")

        for i, category in enumerate(sorted(categories), start=1):
            print(f"{i}. {category}")

        client.disconnect()

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