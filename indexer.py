from mapper import headers_to_articles
from parser import group_articles,is_complete
from database import save_release


class Indexer:
    def __init__(self, client):
        self.client = client

    def index_group(self, group):
        count, first, last, name = self.client.select_group(group)

        print(f"Group: {name}")
        print(f"Articles: {count}")
        print(f"First: {first}")
        print(f"Last: {last}")

        start = max(int(first), int(last) - 100)

        headers = list(self.client.fetch_headers(start, int(last)))
        print(f"Fetched {len(headers)} headers.")

        articles = headers_to_articles(headers)
        releases = group_articles(articles)

        for release in releases.values():
            release["complete"] = is_complete(release)
            save_releaseq(release)
            print(release["name"])
            print(f"Article:{len(release['articles'])}")
            print(f"Size:{release['size']}")
            print(f"Complete:{release['complete']}")
            print()
            