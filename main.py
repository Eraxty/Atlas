from nntp_client import NNTPClient
from mapper import headers_to_articles
from parser import group_articles, is_complete
from database import create_db, save_release, get_releases

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

    count, first, last, name = client.select_group(group)

    print(f"Group: {name}")
    print(f"Articles: {count}")
    print(f"First: {first}")
    print(f"Last: {last}")

    start = max(int(first), int(last) - 100)

    headers = list(client.fetch_headers(start, int(last)))
    print(f"Fetched {len(headers)} headers.")

    articles = headers_to_articles(headers)
    releases = group_articles(articles)

    for release in releases.values():
        save_release(
            release["name"],
            release["size"],
            is_complete(release)
        )

        print(release["name"])
        print(f"Articles: {len(release['articles'])}")
        print(f"Size: {release['size']}")
        print(f"Complete: {is_complete(release)}")
        print()

finally:
    client.disconnect()
