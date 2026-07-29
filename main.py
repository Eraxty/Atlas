from nntp_client import NNTPClient
from database import create_db
from indexer import Indexer
from config import load_config, save_config
from search import search_releases
from nzb import generate_nzb

create_db()

while True:
    choice = input("1. Index\n2. Search\nChoice: ")

    if choice == "1":

        config = load_config()

        if config:
            print(f"Using saved server: {config['host']}")
            print(f"Newsgroup: {config['group']}")

            use_saved = input("Use saved config? (Y/n): ").lower()

            if use_saved == "n":
                host = input("Host: ")
                username = input("Username: ")
                password = input("Password: ")
                group = input("Newsgroup: ")
                port = int(input("Port (563): ") or "563")

                save_config(host, username, password, port, group)
            else:
                host = config["host"]
                username = config["username"]
                password = config["password"]
                group = config["group"]
                port = config["port"]

        else:
            host = input("Host: ")
            username = input("Username: ")
            password = input("Password: ")
            group = input("Newsgroup: ")
            port = int(input("Port (563): ") or "563")

            save_config(host, username, password, port, group)

        while True:
            client = NNTPClient(
                host=host,
                username=username,
                password=password,
                port=port
            )

            try:
                client.connect()
                break

            except Exception as e:
                print(f"\nConnection failed: {e}\n")

                host = input("Host: ")
                username = input("Username: ")
                password = input("Password: ")
                group = input("Newsgroup: ")
                port = int(input("Port (563): ") or "563")

                save_config(host, username, password, port, group)

        try:
            indexer = Indexer(client)
            indexer.index_group(group)

        finally:
            client.disconnect()

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