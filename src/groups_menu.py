from src.nntp_client import NNTPClient
from src.groups import get_categories
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

    categories = get_categories(client)

    while True:
        print("Categories\n")
        names = sorted(categories)

        for i, name in enumerate(names, 1):
            print(f"{i}. {name}")

        print("0. Back")
        category = read_int("Choice: ")

        if category == 0:
            break

        if category < 0 or category > len(names):
            continue

        groups = categories[names[category - 1]]
        print()

        for i, group in enumerate(groups, 1):
            print(f"{i}. {group}")

        print("0. Back")
        group = read_int("Choice: ")

        if group == 0:
            continue

        if group < 0 or group > len(groups):
            continue

        config["group"] = groups[group - 1]

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