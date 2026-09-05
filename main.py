from src.config import load_config, save_config
from src.database import create_db
from src.download import download_release
from src.groups_menu import groups_menu
from src.nzb import generate_nzb
from src.prompts import prompt
from src.sab import rotate_log
from src.search import count_all_releases, count_releases, get_articles, search_all_releases, search_releases
from src.colors import reset, bold, dim, red, green, yellow, cyan
from src.dashboard import load as load_stats, render as render_dashb
from rich.console import Console
from rich.panel import Panel
from rich.table import Table
from rich.text import Text
from rich.console import Group
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
STATS_FILE = BASE_DIR / "stats.json"

console = Console()

#my logo
LOGO = r"""
         █████╗ ████████╗ ██╗       █████╗  ███████╗
        ██╔══██╗╚══██╔══╝ ██║      ██╔══██╗ ██╔════╝
        ███████║   ██║    ██║      ███████║ ███████╗
        ██╔══██║   ██║    ██║      ██╔══██║ ╚════██║
        ██║  ██║   ██║    ███████╗ ██║  ██║ ███████║
        ╚═╝  ╚═╝   ╚═╝    ╚══════╝ ╚═╝  ╚═╝ ╚══════╝
"""


def panel(content, border = "blue"):
    return Panel(content, border_style = border)


def clear():
    os.system("cls" if os.name == "nt" else "clear")


def fmt_size(size):
    if size is None:
        return "?"

    if size <= 0:
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
        PID_FILE.unlink(missing_ok = True)
        return False

    if _is_indexer_pid(pid):
        return True

    PID_FILE.unlink(missing_ok = True)
    return False


def start_background_indexer():
    if indexer_alive():
        console.print("[yellow]indexer already running[/yellow]")
        return False

    try:
        rotate_log(LOG_FILE)
        log_file = LOG_FILE.open("a")

    except OSError as e:
        console.print(f"[red]couldnt start indexer: {e}[/red]")
        return False

    try:
        subprocess.Popen(
            [sys.executable, "-u", "bg_indexer.py"],
            cwd = BASE_DIR,
            stdin = subprocess.DEVNULL,
            stdout = log_file,
            stderr = subprocess.STDOUT,
            start_new_session = True,
        )

    except OSError as e:
        log_file.close()
        console.print(f"[red]couldnt start indexer: {e}[/red]")
        return False

    log_file.close()

    for _ in range(50):
        if indexer_alive():
            return True
        time.sleep(0.1)

    console.print(f"[red]indexer didnt come up, check {LOG_FILE.name}[/red]")
    return False


def stop_background_indexer():
    if not PID_FILE.exists():
        return False

    try:
        pid = int(PID_FILE.read_text().strip())
    except ValueError:
        PID_FILE.unlink(missing_ok = True)
        return False

    if not _is_indexer_pid(pid):
        PID_FILE.unlink(missing_ok = True)
        return False

    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        PID_FILE.unlink(missing_ok = True)
        return False

    #5 sec to comply or die
    for _ in range(50):
        if not _is_indexer_pid(pid):
            PID_FILE.unlink(missing_ok = True)
            return True

        time.sleep(0.1)

    #didnt exit in time soo kill him
    try:
        os.kill(pid, signal.SIGKILL)

    except (ProcessLookupError, PermissionError):
        pass

    PID_FILE.unlink(missing_ok = True)
    return True
    

def ask(text, default = None):
    while True:
        value = prompt(text).strip()

        if not value:
            return default if default is not None else 0

        try:
            return int(value)
        except ValueError:
            console.print("[red]that aint a number[/red]\n")


def show_results(releases, query, page, total_pages, total, page_size):
    clear()

    header = Text()
    header.append(f"Search: {query}\n", style = "bold")
    header.append(f"Page {page + 1} of {total_pages}\n")
    start = page * page_size + 1
    end = min((page + 1) * page_size, total)
    header.append(f"Showing {start}-{end} of {total} results", style = "dim")

    table = Table(show_header = True, header_style = "bold cyan", box = None, padding = (0, 2))
    table.add_column("#", width = 4, justify = "right")
    table.add_column("Name", ratio = 3)
    table.add_column("Size", width = 10, justify = "right")
    table.add_column("Parts", width = 8, justify = "right")
    table.add_column("Date", width = 12)
    table.add_column("Status", width = 10)

    width = max(20, shutil.get_terminal_size().columns - 40)

    for i, release in enumerate(releases, 1):
        name = release[1]

        if len(name) > width:
            name = name[:width - 3] + "..."

        broken = "[red][broken][/red]" if not release[6] else ""

        table.add_row(
            str(i),
            name,
            fmt_size(release[5]),
            str(release[7]),
            fmt_date(release[4]),
            broken
        )

    console.print(panel(header, "cyan"))
    console.print(table)
    console.print("\n[dim]0. Back[/dim]")

    if page > 0:
        console.print("[cyan]p.[/cyan] Previous Page")
    if page < total_pages - 1:
        console.print("[cyan]n.[/cyan] Next Page")
    console.print("[cyan]g.[/cyan] Go to Page")


def show_release(release, articles):
    info = Text()
    info.append(f"Release: {release[1]}\n", style = "bold")
    info.append(f"poster: {release[3] or 'unknown'}   posted: {fmt_date(release[4])}\n", style = "dim")
    info.append(f"{fmt_size(release[5])} - {release[7]} parts - ")
  
    if release[6]:
        info.append("complete", style = "green")
    else:
        info.append("incomplete", style = "red")
    info.append("\n")

    console.print(panel(info))

    if not articles:
        return

    files = {}

    for a in articles:
        files.setdefault(a[1] or "?", []).append(a)

    table = Table(title = f"files ({len(files)})", show_header = False, box = None, padding = (0, 1))
    table.add_column("name", style = "white")
    table.add_column("parts", style = "dim")

    shown = list(files.items())[:30]
    width = max(20, shutil.get_terminal_size().columns - 20)

    for filename, parts in shown:
        present = len({p[2] for p in parts})
        expected = max((p[3] for p in parts if p[3]), default = present)
        name = filename if len(filename) <= width else filename[:width - 3] + "..."
        table.add_row(name, f"{present}/{expected}")

    console.print(table)

    if len(files) > len(shown):
        console.print(f"  [dim]... and {len(files) - len(shown)} more[/dim]")


def setup():
    console.print(panel("[red]no config found[/red]", "red"))

    host = prompt("Host: ")
    username = prompt("Username: ")
    password = prompt("Password: ")
    port = ask("Port (563): ", 563)

    save_config(host, username, password, port, "")
    console.print(panel("[yellow]group empty rn, select one from the Groups menu[/yellow]", "yellow"))


def do_search(config):
    while True:
        clear()

        menu = Table(show_header = False, box = None, padding = (0, 2))
        menu.add_column("num", style = "bold cyan", width = 3)
        menu.add_column("label", style = "white")
        menu.add_row("1.", "Current Group")
        menu.add_row("2.", "All Groups")
        menu.add_row("0.", "Back")
        console.print(panel(menu, "green"))

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
                console.print(panel("[red]couldnt search, db error[/red]", "red"))
                return

            if not total:
                console.print(panel("[red]no releases found[/red]", "red"))
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
                    console.print("[dim]already on the first page[/dim]")
                    prompt("[enter]")
                continue

            if choice == "n":
                if page < total_pages - 1:
                    page += 1
                else:
                    console.print("[dim]already on the last page[/dim]")
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
                    console.print(f"[red]page must be between 1 and {total_pages}[/red]")
                    prompt("[enter]")
                continue

            try:
                selected = int(choice)
            except ValueError:
                console.print("[red]invalid[/red]")
                prompt("[enter]")
                continue

            if selected < 1 or selected > len(releases):
                console.print("[red]not on this page[/red]")
                prompt("[enter]")
                continue

            release = releases[selected - 1]
            choice_id = release[0]
            articles = get_articles(choice_id)

            while True:
                clear()

                #actions for the picked release
                show_release(release, articles)

                menu = Table(show_header = False, box = None, padding = (0, 2))
                menu.add_column("num", style = "bold cyan", width = 3)
                menu.add_column("label", style = "white")
                menu.add_row("1.", "Download")
                menu.add_row("2.", "Save NZB")
                menu.add_row("0.", "Back")
                console.print(menu)

                choice = prompt("\nChoice: ").strip()

                if choice == "1":
                    try:
                        ok = download_release(choice_id)

                    except Exception as e:
                        console.print(f"[red]couldnt queue download: {e}[/red]")
                        ok = False

                    if ok:
                        console.print(panel("[green]Download queued it is downloading in background.[/green]\n[dim]Finished files land ~/Downloads[/dim]", "green"))
                    prompt("[enter]")
                    break

                if choice == "2":
                    try:
                        generate_nzb(choice_id)
                    except Exception as e:
                        console.print(f"[red]couldnt save nzb: {e}[/red]")
                    prompt("[enter]")
                    break
                if choice == "0":
                    break

                console.print("[red]invalid[/red]")
                prompt("[enter]")


def do_settings():
    clear()

    config = load_config()

    menu = Table(show_header = False, box = None, padding = (0, 2))
    menu.add_column("num", style = "bold cyan", width = 3)
    menu.add_column("label", style = "white")
    menu.add_row("1.", "Change config")
    menu.add_row("2.", f"Change indexer mode ({config.get('index_mode', 'dynamic')})")
    menu.add_row("0.", "Back")
    console.print(panel(menu, "blue"))

    choice = ask("\nChoice: ")

    if choice == 2:
        while True:
            clear()

            #modes
            modes = {1: "dynamic", 2: "live", 3: "backfill"}

            menu = Table(show_header = False, box = None, padding = (0, 2))
            menu.add_column("num", style = "bold cyan", width = 3)
            menu.add_column("label", style = "white")
            
            for k, v in modes.items():
                menu.add_row(str(k), v)
            
            menu.add_row("0.", "Back")
            console.print(panel(menu, "blue"))

            mode = ask("\nChoice: ")

            if mode == 0:
                return

            if mode not in modes:
                console.print("[red]that is not a number[/red]")
                continue

            save_config(
                config["host"],
                config["username"],
                config.get("password", ""),
                config["port"],
                config["group"],
                modes[mode],
            )

            console.print(f"[green]indexer mode set to {modes[mode]}[/green]")
            return

    if choice != 1:
        return

    host = prompt(f"Host ({config['host']}): ").strip() or config["host"]
    username = prompt(f"Username ({config['username']}): ").strip() or config["username"]
    password = prompt("Password: ").strip() or config.get("password", "")

    while True:
        group = prompt("Newsgroup: ").strip()

        if not group:
            group = config["group"]
        if group:
            break
        console.print("[yellow]newsgroup cant be empty[/yellow]\n")

    port = ask(f"Port ({config['port']}): ", config["port"])

    save_config(host, username, password, port, group, config.get("index_mode", "dynamic"))
    
    console.print("[green]saved[/green]")


def main():
    create_db()

    config = load_config()

    if config:
        console.print(panel(
            f"[green]config loaded[/green]\n"
            f"Server: {config['host']}\n"
            f"Current Group: {config['group']}",
            "cyan"
        ))
    else:
        setup()
        config = load_config()

        if not config:
            console.print("[red]setup failed, no config found[/red]")
            return

    while True:
        clear()

        indexing = indexer_alive()
        status = get_status()

        #build the status line
        st = status.get("status", "stopped")
        label = status.get("group") or config["group"]
        err_count = status.get("error_count", 0)

        idx_text = Text()
        if indexing:
            if st == "warning":
                extra = f" ({err_count} errors)" if err_count else ""
                idx_text.append(f"{label} ", style = "yellow")
                idx_text.append("[WARNING]", style = "yellow bold")
                idx_text.append(extra, style = "yellow")
            elif status.get("idle"):
                idx_text.append(f"{label} (idle)", style = "cyan")
            else:
                idx_text.append(f"{label} ", style = "green")
                idx_text.append("[active]", style = "green bold")
        elif st == "error":
            if status.get("stale"):
                idx_text.append("stopped (last run failed)", style = "dim")
            else:
                idx_text.append("FAILED (error)", style = "red bold")
        elif st == "warning":
            if status.get("stale"):
                idx_text.append("stopped (last run: warning)", style = "dim")
            else:
                idx_text.append("stopped (warning)", style = "yellow")
        else:
            idx_text.append("stopped", style = "dim")

        content = Text()
        content.append(Text(LOGO, style = "bold cyan"))
        content.append("\n")
        content.append("Current Group : ", style = "bold")
        content.append(config["group"])
        content.append("\nIndexing      : ", style = "bold")
        content.append_text(idx_text)

        menu = Table(show_header = False, box = None, padding = (0, 2))
        menu.add_column("num", style = "bold cyan", width = 2)
        menu.add_column("label", style = "white")

        groups = config.get("groups") or []

        if indexing:
            menu.add_row("1.", "Stop Indexing")
        else:
            menu.add_row("1.", "Start Indexing")

        menu.add_row("2.", "Search")
        menu.add_row("3.", "Groups")
        if len(groups) > 1:
            menu.add_row("4.", "Remove group")
        else:
            menu.add_row("4.", "Settings")
        menu.add_row("5.", "Live Dashboard")
        menu.add_row("0.", "Exit")

        full = Group(
            content,
            "",
            menu,
        )

        console.print(panel(full, "cyan"))

        choice = prompt("\nChoice: ")

        if choice == "1":
            if indexing:
                stopped = stop_background_indexer()
                console.print("[green]indexing stopped[/green]" if stopped else "[yellow]indexer wasnt running[/yellow]")
            else:
                if start_background_indexer():
                    console.print("[green]indexing started[/green]")

        elif choice == "2":
            do_search(config)

        elif choice == "3":
            groups_menu(config)
            config = load_config()

        elif choice == "4":
            groups = config.get("groups") or []
            
            if len(groups) > 1:
                page = 0
            
                while True:
                    clear()

                    start = page * 5
                    end = min(start + 5, len(groups))
                    total_pages = max(1, (len(groups) + 4) // 5)

                    console.print(panel(
                        f"[bold]Remove group[/bold]\n"
                        f"Page {page + 1} of {total_pages}\n"
                        f"[dim]Showing {start + 1}-{end} of {len(groups)} groups[/dim]",
                        "red"
                    ))

                    table = Table(show_header = True, header_style = "bold cyan", box = None, padding = (0, 2))
                    table.add_column("#", width = 4, justify = "right")
                    table.add_column("Group", ratio = 1)

                    for i, group in enumerate(groups[start:end], 1):
                        table.add_row(str(i), group)

                    console.print(table)
                    console.print("\n[dim]0. Back[/dim]")

                    if page > 0:
                        console.print("[cyan]p.[/cyan] Previous Page")
            
                    if end < len(groups):
                        console.print("[cyan]n.[/cyan] Next Page")

                    choice = prompt("\nChoice: ").strip()

                    if choice == "0":
                        break

                    if choice == "p":
                        if page > 0:
                            page -= 1
                        else:
                            console.print("[dim]already on the first page[/dim]")
                            prompt("[enter]")
                        continue

                    if choice == "n":
                        if end < len(groups):
                            page += 1
                        else:
                            console.print("[dim]already on the last page[/dim]")
                            prompt("[enter]")
                        continue

                    try:
                        selected = int(choice)
                    except ValueError:
                        console.print("[red]invalid[/red]")
                        prompt("[enter]")
                        continue

                    if selected < 1 or selected > end - start:
                        console.print("[red]invalid[/red]")
                        prompt("[enter]")
                        continue

                    chosen = groups[start + selected - 1]
                    config["groups"] = [g for g in config["groups"] if g != chosen]

                    save_config(
                        config["host"],
                        config["username"],
                        config.get("password", ""),
                        config["port"],
                        config["group"],
                        config.get("index_mode", "dynamic"),
                        config["groups"],
                    )
                    console.print(f"[green]removed {chosen}[/green]")
                    prompt("[enter]")
                    break
            else:
                do_settings()
                config = load_config()

        elif choice == "5":
            from rich.live import Live
            try:
                with Live(render_dashb(None), console = console, refresh_per_second = 4, screen = True) as live:
                    while True:
                        live.update(render_dashb(load_stats()))
                        time.sleep(0.25)
            
            except KeyboardInterrupt:
                pass

        elif choice == "0":
            #byee
            console.print("\n[bold cyan]byee.[/bold cyan]")
            break


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        #byee
        print("\nbyeee")
