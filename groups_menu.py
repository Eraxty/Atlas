from nntp_client import NNTPClient
from groups import get_categories
from config import save_config
from prompts import prompt


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
        category = int(prompt("Choice: "))

        if category == 0:
            break

        if category > len(names):
            continue

        groups = categories[names[category - 1]]
        print()

        for i, group in enumerate(groups, 1):
            print(f"{i}. {group}")

        print("0. Back")
        group = int(prompt("Choice: "))

        if group == 0:
            continue

        if group > len(groups):
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