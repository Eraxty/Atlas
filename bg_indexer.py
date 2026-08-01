from config import load_config
from nntp_client import NNTPClient
from indexer import Indexer
import time

config = load_config()

client = NNTPClient(
    host=config["host"],
    username=config["username"],
    password=config["password"],
    port=config["port"],
)

client.connect()

indexer = Indexer(client)

while True:
    config = load_config()
    indexer.index_group(config["group"])
    time.sleep(0.1)
