from dataclasses import dataclass
from parser import group_articles, is_complete
from database import create_db, save_release, get_releases
from nntp_client import NNTPClient

@dataclass
class Article:
    number: int
    subject: str
    author: str
    date: str
    message_id: str
    references: str
    bytes: int
    lines: int

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

count, first, last, name = client.select_group(group)

print(f"Group: {name}")
print(f"Articles: {count}")
print(f"First: {first}")
print(f"Last: {last}")

start = max(int(first), int(last) - 100)

headers = list(client.fetch_headers(start, int(last)))
print(f"Fetched {len(headers)} headers.")

for number, header in headers[:5]:
    print(number)
    print(header)

client.disconnect()