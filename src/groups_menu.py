from src.nntp_client import NNTPClient
from src.config import save_config
from src.parser import parse_subject
from src.prompts import prompt


def read_int(text):
    while True:
        try:
            return int(prompt(text))
        except ValueError:
            print("Invalid input, please enter a number.\n")


def groups_menu(config):
    client = NNTPClient(
        config["host"],
        config["username"],
        config["password"],
        config["port"]
    )

    client.connect()

    groups = []

    for line in client.list_groups():
        line = line.strip()

        if line:
            groups.append(line.split()[0])

    while True:
        query = prompt("Search groups: ").strip()

        if not query:
            break

        if len(query) < 3:
            print("Search atleast 3 characters\n")
            continue

        matches = [group for group in groups if query.lower() in group.lower()]

        if not matches:
            print("No matching groups found\n")
            continue

        if len(matches) > 30:
            print(f"{len(matches)} top 30 matches shown \n")
            matches = matches[:30]

        print()

        for i, group in enumerate(matches, 1):
            print(f"{i}. {group}")

        print("0. Back")
        choice = read_int("Choice: ")

        if choice == 0:
            continue

        if choice < 0 or choice > len(matches):
            continue

        config["group"] = matches[choice - 1]

        count, first, last, _ = client.select_group(config["group"])

        if last > first:
            headers = list(client.fetch_headers(max(first, last - 49), last))
            parsed = sum(1 for _, header in headers if parse_subject(header["subject"]))

            if parsed == 0:
                answer = prompt(f"{parsed}/{len(headers)} subjects look like binary posts — this could be a text/discussion group, not a binaries group. Index anyway? (y/n) ").strip().lower()

                if answer not in ("y", "yes"):
                    continue

        save_config(
            config["host"],
            config["username"],
            config["password"],
            config["port"],
            config["group"]
        )

        client.disconnect()
        return

    client.disconnect()