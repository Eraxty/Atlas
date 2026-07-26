from nntp_client import NNTPClient
from database import create_db
from indexer import Indexer
from config import load_config, save_config

create_db()

config = load_config()

if config is None:
    host = input("Host: ")
    username = input("Username: ")
    password = input("Password: ")
    group = input("Newsgroup: ")
    port = int(input("Port default (563): ") or "563")

    save_config(host, username, password, port, group)
else:
    host = config["host"]
    username = config["username"]
    password = config["password"]
    group = config["group"]
    port = config["port"]

client = NNTPClient(
    host=host,
    username=username,
    password=password,
    port=port
)

client.connect()

try:
    indexer = Indexer(client)
    indexer.index_group(group)

finally:
    client.disconnect()