import os
import nntp

from src.nntp_client import NNTPClient
from src.config import save_config
from src.parser import parse_subject
from src.prompts import prompt
from src.colors import red, green, yellow, dim, reset


def clear():
    os.system("cls" if os.name == "nt" else "clear")


_groups_cache = {}


def load_groups(client, host):
    if host in _groups_cache:
        return _groups_cache[host]

    groups = []

    for line in client.list_groups():
        line = line.strip()

        if line:
            groups.append(line.split()[0])

    _groups_cache[host] = groups
    return groups


def groups_menu(config):
    if not config.get("host"):
        print(f"{red}no server configured, setup in Settings first{reset}")
        prompt("[enter]")
        return

    host = config["host"]

    client = NNTPClient(
        config["host"],
        config["username"],
        config["password"],
        config["port"]
    )

    if host not in _groups_cache:
        try:
            client.connect()

        except (OSError, nntp.NNTPReplyError):
            print(f"{red}couldnt connect to server{reset}")
            prompt("[enter]")
            return

    try:
        groups = load_groups(client, host)

    except (OSError, nntp.NNTPError):
        print(f"{red}couldnt fetch groups from the server{reset}")
        prompt("[enter]")
        client.disconnect()
        return

    while True:
        clear()

        query = prompt("Search groups: ").strip()

        if not query:
            break

        #"something all" includes every group, not just binaries
        if query.lower().endswith(" all"):
            search_all = True
            query = query[:-4].strip()
        else:
            search_all = False

        if len(query) < 3:
            print(f"{yellow}Search atleast 3 characters bruh{reset}\n")
            prompt("[enter]")
            continue

        matches = [group for group in groups if query.lower() in group.lower()]

        #default skips text groups
        if not search_all:
            matches = [group for group in matches if ".binaries." in group.lower()]

        if not matches:
            print(f"{red}No matching groups found{reset}\n")
            prompt("[enter]")
            continue

        page = 0

        while True:
            clear()

            start = page * 30
            end = min(start + 30, len(matches))
            #30 per page
            total_pages = max(1, (len(matches) + 29) // 30)

            print(f"Page {page + 1} of {total_pages}")
            print(f"{dim}Showing {start + 1}-{end} of {len(matches)} matches{reset}\n")

            for i, group in enumerate(matches[start:end], 1):
                print(f"{i}. {group}")

            print("0. Back")

            if page > 0:
                print("p. Previous Page")
            if end < len(matches):
                print("n. Next Page")

            choice = prompt("\nChoice: ").strip()

            if choice == "0":
                break

            if choice == "p":
                if page > 0:
                    page -= 1
                else:
                    print(f"{dim}already on the first page{reset}")
                    prompt("[enter]")
                continue

            if choice == "n":
                if end < len(matches):
                    page += 1
                else:
                    print(f"{dim}already on the last page{reset}")
                    prompt("[enter]")
                continue

            try:
                selected = int(choice)
            except ValueError:
                print(f"{red}invalid{reset}")
                prompt("[enter]")
                continue

            #choices count from 1 on the current page only
            if selected < 1 or selected > end - start:
                print(f"{red}invalid{reset}")
                prompt("[enter]")
                continue

            #map the page choice back to the full list index
            config["group"] = matches[start + selected - 1]

            if not client.server:
                try:
                    client.connect()

                except (OSError, nntp.NNTPReplyError):
                    print(f"{red}couldnt connect to server{reset}")
                    prompt("[enter]")
                    continue

            try:
                count, first, last, _ = client.select_group(config["group"])

            except (OSError, nntp.NNTPError) as e:
                print(f"{red}couldnt select group: {e}{reset}")
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
                        answer = prompt(f"only {parsed}/{len(headers)} look like binaries, probs a text group. index anyway? (y/n) ").strip().lower()

                        if answer not in ("y", "yes"):
                            continue

            #save the pick soo it sticks after restart
            save_config(
                config["host"],
                config["username"],
                config["password"],
                config["port"],
                config["group"],
                config.get("index_mode", "dynamic")
            )

            client.disconnect()
            return

    client.disconnect()
