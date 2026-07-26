from nntp_client import NNTPClient
from database import create_db
from indexer import Indexer

host = input("Host: ")
username = input("Username: ")
password = input("Password: ")
group = input("Newsgroup: ")
port = int(input("Port default:(563): <Press enter>") or "563")

client = NNTPClient(
    host=host,
    username=username,
    password=password,
    port=port
)

client.connect()

try:
    create_db()

    indexer = Indexer(client)
    indexer.index_group(group)

finally:
    client.disconnect()