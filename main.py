from nntp_client import NNTPClient
from database import create_db
from indexer import Indexer
from config import load_config, save_config

create_db()

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