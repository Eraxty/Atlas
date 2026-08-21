from src.config import load_config, save_config
from src.database import create_db
from src.download import download_release
from src.groups_menu import groups_menu
from src.nzb import generate_nzb
from src.prompts import prompt
from src.sab import rotate_log
from src.search import count_all_releases, count_releases, get_articles, search_all_releases, search_releases
from src.colors import reset, bold, dim, red, green, yellow, cyan
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
            status = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {"running": False, "group": ""}

    pid = status.get("pid")

    status["stale"] = not (isinstance(pid, int) and _is_indexer_pid(pid))

    return status


def _pid_state(pid):
    try:
        data = Path(f"/proc/{pid}/stat").read_bytes()
    except OSError:
        return None

    end = data.rfind(b")")

    if end == -1 or end + 2 >= len(data):
        return None

    return chr(data[end + 2])


def _is_indexer_pid(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False

    except PermissionError:
        return True

    if Path("/proc").exists():
        try:
            cmdline = Path(f"/proc/{pid}/cmdline").read_bytes()

        except OSError:
            return False

        return b"bg_indexer.py" in cmdline

    return True


def indexer_alive():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
    except ValueError:
        PID_FILE.unlink(missing_ok=True)
        return False

    if _is_indexer_pid(pid):
        return True

    PID_FILE.unlink(missing_ok=True)
    return False


def start_background_indexer():
    if indexer_alive():
        print(f"{yellow}indexer already running{reset}")
        return False

    try:
        rotate_log(LOG_FILE)

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
        print(f"{red}couldnt start indexer: {e}{reset}")
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

    if not _is_indexer_pid(pid):
        PID_FILE.unlink(missing_ok = True)
        return False

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        PID_FILE.unlink(missing_ok=True)
        return False

    #5 sec to comply or die
    for _ in range(50):
        if not _is_indexer_pid(pid):
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

        if not value:
            return default if default is not None else 0

        try:
            return int(value)
        except ValueError:
            print(f"{red}that aint a number{reset}\n")


def show_results(releases, query, page, total_pages, total, page_size):
    clear()

    print(f"Search: {query}")
    print(f"Page {page + 1} of {total_pages}")

    start = page * page_size + 1
    end = min((page + 1) * page_size, total)
    print(f"{dim}Showing {start}-{end} of {total} results{reset}\n")

    width = max(20, shutil.get_terminal_size().columns - 40)

    for i, release in enumerate(releases, 1):
        name = release[1]

        if len(name) > width:
            name = name[:width - 3] + "..."

        broken = f"  {red}[broken]{reset}" if not release[6] else ""

        print(f"{i}. {name}  {fmt_size(release[5])} - {release[7]} parts - {fmt_date(release[4])}{broken}")

    print("\n0. Back")

    if page > 0:
        print("p. Previous Page")
    if page < total_pages - 1:
        print("n. Next Page")

    print("g. Go to Page")


def show_release(release, articles):
    print(f"Release: {release[1]}")
    print(f"{dim}poster: {release[3] or 'unknown'}   posted: {fmt_date(release[4])}{reset}")

    status = f"{green}complete{reset}" if release[6] else f"{red}incomplete{reset}"
    print(f"{fmt_size(release[5])} - {release[7]} parts - {status}\n")

    if not articles:
        return

    files = {}

    for a in articles:
        files.setdefault(a[1] or "?", []).append(a)

    width = max(20, shutil.get_terminal_size().columns - 20)

    print(f"files ({len(files)}):")

    shown = list(files.items())[:30]

    for filename, parts in shown:
        present = len({p[2] for p in parts})
        expected = max((p[3] for p in parts if p[3]), default = present)

        name = filename if len(filename) <= width else filename[:width - 3] + "..."

        print(f"  {name}  {dim}{present}/{expected}{reset}")

    if len(files) > len(shown):
        print(f"  {dim}... and {len(files) - len(shown)} more{reset}")

    print()


def setup():
    print(f"{red}no config found{reset}")

    host = prompt("Host: ")
    username = prompt("Username: ")
    password = prompt("Password: ")
    port = ask("Port (563): ", 563)

    save_config(host, username, password, port, "")
    print(f"\n{yellow}group empty rn, select one from the Groups menu{reset}")


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
                print(f"\n{red}couldnt search, db error{reset}")
                return

            if not total:
                print(f"\n{red}no releases found{reset}")
                query = prompt("\nSearch: ").strip()
                if not query or query == "0":
                    break
                page = 0
                continue

            total_pages = max(1, math.ceil(total / page_size))

            if page > total_pages - 1:
                page = total_pages - 1
                continue

            show_results(releases, query, page, total_pages, total, page_size)

            choice = prompt("\nChoice: ").strip()

            if choice == "0":
                return

            if choice == "p":
                if page > 0:
                    page -= 1
                else:
                    print(f"{dim}already on the first page{reset}")
                    prompt("[enter]")
                continue

            if choice == "n":
                if page < total_pages - 1:
                    page += 1
                else:
                    print(f"{dim}already on the last page{reset}")
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
                    print(f"{red}page must be between 1 and {total_pages}{reset}")
                    prompt("[enter]")
                continue

            try:
                selected = int(choice)
            except ValueError:
                print(f"{red}invalid{reset}")
                prompt("[enter]")
                continue

            if selected < 1 or selected > len(releases):
                print(f"{red}not on this page{reset}")
                prompt("[enter]")
                continue

            release = releases[selected - 1]
            choice_id = release[0]
            articles = get_articles(choice_id)

            while True:
                clear()

                #actions for the picked release
                show_release(release, articles)

                print("1. Download")
                print("2. Save NZB")
                print("0. Back")

                choice = prompt("\nChoice: ").strip()

                if choice == "1":
                    try:
                        ok = download_release(choice_id)
                    
                    except Exception as e:
                        print(f"{red}couldnt queue download: {e}{reset}")
                        ok = False

                    if ok:
                        print(f"\n{green}Download queued it is downloading in background.{reset}")
                        print(f"{dim}Finished files land ~/Downloads{reset}")
                    prompt("[enter]")
                    break

                if choice == "2":
                    try:
                        generate_nzb(choice_id)
                    except Exception as e:
                        print(f"{red}couldnt save nzb: {e}{reset}")
                    prompt("[enter]")
                    break
                if choice == "0":
                    break

                print(f"{red}invalid{reset}")
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
                print(f"{red}that is not a number{reset}")
                continue

            save_config(
                config["host"],
                config["username"],
                config.get("password", ""),
                config["port"],
                config["group"],
                modes[mode],
            )

            print(f"{green}indexer mode set to {modes[mode]}{reset}")
            return

    if choice != 1:
        return

    print()
    host = prompt(f"Host ({config['host']}): ").strip() or config["host"]
    username = prompt(f"Username ({config['username']}): ").strip() or config["username"]
    password = prompt("Password: ").strip() or config.get("password", "")

    while True:
        group = prompt("Newsgroup: ").strip()

        if not group:
            group = config["group"]
        if group:
            break
        print(f"{yellow}newsgroup cant be empty{reset}\n")

    port = ask(f"Port ({config['port']}): ", config["port"])

    save_config(host, username, password, port, group, config.get("index_mode", "dynamic"))
    
    print(f"\n{green}saved{reset}")


def main():
    create_db()

    config = load_config()

    if config:
        print(f"{green}config loaded:{reset}")
        print(f"Server: {config['host']}")
        print(f"Current Group: {config['group']}\n")
    else:
        setup()
        config = load_config()

        if not config:
            print(f"{red}setup failed, no config found {reset}")
            return

    while True:
        clear()

        indexing = indexer_alive()
        status = get_status()

        print("=" * 55)
        print(f"{cyan}{bold}{LOGO}{reset}")
        print("=" * 55)
        print(f"Current Group : {config['group']}")

        #build the status line
        st = status.get("status", "stopped")
        label = status.get("group") or config["group"]
        err_count = status.get("error_count", 0)

        if indexing:
            if st == "warning":
                #yellow warning - errors happening but still running
                indicator = f"{yellow}WARNING{reset}"
                extra = f" ({err_count} errors)" if err_count else ""
                print(f"Indexing      : {yellow}{label}{reset} {dim}[{indicator}]{reset}{extra}")
            elif status.get("idle"):
                #cyan for idle - alive but waiting
                print(f"Indexing      : {cyan}{label} (idle){reset}")
            else:
                #green - actively running
                print(f"Indexing      : {green}{label} [active]{reset}")
        
        elif st == "error":
            if status.get("stale"):
                print(f"Indexing      : {dim}stopped (last run failed){reset}")
            else:
                print(f"Indexing      : {red}FAILED (error){reset}")
        
        elif st == "warning":
            if status.get("stale"):
                print(f"Indexing      : {dim}stopped (last run: warning){reset}")
            else:
                print(f"Indexing      : {yellow}stopped (warning){reset}")
        
        else:
            print(f"Indexing      : {dim}stopped{reset}")

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
                print(f"{green}indexing stopped{reset}" if stopped else f"{yellow}indexer wasnt running{reset}")
            else:
                if start_background_indexer():
                    print(f"{green}indexing started{reset}")

        elif choice == "2":
            do_search(config)

        elif choice == "3":
            groups_menu(config)
            config = load_config()

        elif choice == "4":
            do_settings()
            config = load_config()

        elif choice == "0":
            #byee
            print("\nbyee.")
            break


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        #byee
        print("\nbyeee")
