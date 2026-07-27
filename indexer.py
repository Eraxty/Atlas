from mapper import headers_to_articles
from parser import group_articles, is_complete
from database import save_release, get_last_article, update_last_article 


class Indexer:
    def __init__(self, client):
        self.client = client

    def index_group(self, group):
        count, first, last, name = self.client.select_group(group)

        print(f"Group: {name}")
        print(f"Articles: {count}")
        print(f"First: {first}")
        print(f"Last: {last}")

        last_saved = get_last_article(group)

        if last_saved is None:
            start = max(int(first), int(last)-100)
        else:
            start = last_saved + 1

        headers = list(self.client.fetch_headers(start, int(last)))
        print(f"Fetched {len(headers)} headers.")

        articles = headers_to_articles(headers)
        releases = group_articles(articles)
        
        for release in releases.values():
            release["complete"] = is_complete(release)
            release["group"] = group
            print(type(release["articles"][0]))
            print(release["articles"][0])
            release["poster"] = release["articles"][0].author
            release["date"] = release["articles"][0].date

            save_release(release)

            print(release["name"])
            print(f"Articles: {len(release['articles'])}")
            print(f"Size: {release['size']}")
            print(f"Complete: {release['complete']}")
            print()
        
        update_last_article(group, int(last))
