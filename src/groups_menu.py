import os
import time
import nntp

from src.nntp_client import NNTPClient
from src.config import save_config
from src.parser import parse_subject
from src.prompts import prompt
from src.colors import red, yellow, dim, reset
from rich.console import Console
from rich.panel import Panel
from rich.table import Table

console = Console()


def panel(content, border = "blue"):
    return Panel(content, border_style = border)


def clear():
    os.system("cls" if os.name == "nt" else "clear")


_groups_cache = {}


def load_groups(client, host, pattern = None):
    cache_key = (host, pattern)
    #refetch every 10 min so new groups show up without a restart
    cached = _groups_cache.get(cache_key)

    if cached and time.time() - cached[0] < 600:
        return cached[1]

    groups = []

    try:
        for item in client.list_groups(pattern):
            if isinstance(item, tuple):
                groups.append(item[0])
            else:
                item = item.strip()
                if item:
                    groups.append(item.split()[0])

    except (OSError, nntp.NNTPError):
        #server doesnt support wildcards soo load everything and filter client side
        if pattern:
            return load_groups(client, host, None)
        raise

    _groups_cache[cache_key] = (time.time(), groups)
    return groups


def groups_menu(config):
    if not config.get("host"):
        console.print("[red]no server configured, setup in Settings first[/red]")
        prompt("[enter]")
        return

    host = config["host"]

    client = NNTPClient(
        config["host"],
        config["username"],
        config.get("password", ""),
        config["port"]
    )

    try:
        #always connect before talking to the server, cache or not
        if not client.server:
            client.connect()

    except (OSError, nntp.NNTPReplyError):
        console.print("[red]couldnt connect to server[/red]")
        prompt("[enter]")
        return

    try:
        while True:
            clear()

            console.print(panel("[bold cyan]Groups[/bold cyan]", "cyan"))
            console.print("[dim]Search for binary groups to add.[/dim]\n")

            query = prompt("Search: ").strip()

            if not query:
                break

            if len(query) < 3:
                console.print("[yellow]Search at least 3 characters bruh[/yellow]\n")
                prompt("[enter]")
                continue

            #build a server side wildcard pattern for fast filtering
            pattern = f"*{query}*"

            try:
                groups = load_groups(client, host, pattern)

            except (OSError, nntp.NNTPError):
                console.print("[red]couldnt fetch groups from the server[/red]")
                prompt("[enter]")
                continue

            #client side filtering if server doesnt support wildcards
            groups = [g for g in groups if query.lower() in g.lower()]

            #default skips text groups
            if not search_all:
                groups = [g for g in groups if ".binaries." in g.lower()]

            if not groups:
                console.print("[red]No matching groups found[/red]\n")
                prompt("[enter]")
                continue

            page = 0

            while True:
                clear()

                start = page * 30
                end = min(start + 30, len(groups))
                #30 per page
                total_pages = max(1, (len(groups) + 29) // 30)

                console.print(panel(
                    f"[bold]Results[/bold]\n"
                    f"Page {page + 1} of {total_pages}\n"
                    f"[dim]Showing {start + 1}-{end} of {len(groups)} matches[/dim]",
                    "green"
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
                if total_pages > 1:
                    console.print("[cyan]g.[/cyan] Go to Page")

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

                #choices count from 1 on the current page only
                if selected < 1 or selected > end - start:
                    console.print("[red]invalid[/red]")
                    prompt("[enter]")
                    continue

                chosen = groups[start + selected - 1]

                if not client.server:
                    try:
                        client.connect()

                    except (OSError, nntp.NNTPReplyError):
                        console.print("[red]couldnt connect to server[/red]")
                        prompt("[enter]")
                        continue

                try:
                    count, first, last, _ = client.select_group(config["group"])

                except (OSError, nntp.NNTPError):
                    client.disconnect()

                    try:
                        client.connect()
                        count, first, last, _ = client.select_group(config["group"])

                    except (OSError, nntp.NNTPError) as e:
                        console.print(f"[red]couldnt select group: {e}[/red]")
                        prompt("[enter]")
                        continue

                #empty group has last == first soo skip the sample
                if last > first:
                    try:
                        #sample the last 50 posts to see what kinda group it is
                        headers = list(client.fetch_headers(max(first, last - 49), last))
                    except nntp.NNTPTemporaryError:
                        answer = prompt("cant sample this group (empty range?). index anyway? (y/n) ").strip().lower()

                        if answer not in ("y", "yes"):
                            continue
                    else:
                        for _, header in headers: #how many of the sample look like binary releases
                            header.setdefault("subject", "")
                        parsed = sum(1 for _, header in headers if parse_subject(header["subject"]))

                        if parsed == 0:
                            answer = prompt(f"none of {len(headers)} look like binaries, probs a text group. index anyway? (y/n) ").strip().lower()

                            if answer not in ("y", "yes"):
                                continue

                #only add the group after user confirms they want to index it
                config["group"] = chosen
                config["groups"] = list(dict.fromkeys((config.get("groups") or []) + [chosen]))

                #save the pick soo it sticks after restart
                save_config(
                    config["host"],
                    config["username"],
                    config.get("password", ""),
                    config["port"],
                    config["group"],
                    config.get("index_mode", "dynamic"),
                    config.get("groups")
                )

                return

    finally:
        client.disconnect()
