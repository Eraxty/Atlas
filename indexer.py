from mapper import headers_to_articles
from parser import group_articles, is_complete
from database import save_release, get_group_state, update_live_cursor, update_backfill_cursor

BACKFILL_SIZE = 1000


class Indexer:
    def __init__(self, client):
        self.client = client

    def index_group(self, group):
        count, first, last, name = self.client.select_group(group)

        print(f"Group: {name}")
        print(f"Articles: {count}")
        print(f"First: {first}")
        print(f"Last: {last}")

        state = get_group_state(group)

        if state is None:
            live_cursor = int(last)
            backfill_cursor = max(int(first), int(last) - BACKFILL_SIZE)

            update_live_cursor(group, live_cursor)
            update_backfill_cursor(group, backfill_cursor)
            return

        self.live(group, int(last))
        self.backfill(group, int(first))

    def live(self, group, last):
        state = get_group_state(group)
        start = state["live_cursor"] + 1

        if start > last:
            print("No new articles.")
            return

        self.process_range(group, start, last)
        update_live_cursor(group, last)

    def backfill(self, group, first):
        state = get_group_state(group)
        end = state["backfill_cursor"]

        if end < first:
            return

        start = max(end - BACKFILL_SIZE, first)
        self.process_range(group, start, end)
        update_backfill_cursor(group, start - 1)

    def process_range(self, group, start, end):
        headers = list(self.client.fetch_headers(start, end))

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
