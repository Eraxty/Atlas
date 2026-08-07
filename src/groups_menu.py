from src.nntp_client import NNTPClient
from src.config import save_config
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

        matches = [group for group in groups if query.lower() in group.lower()]
        matches = matches[:30]

        if not matches:
            print("No matching groups.\n")
            continue

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