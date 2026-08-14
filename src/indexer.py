import nntp

from src.mapper import headers_to_articles
from src.parser import group_articles, is_complete
from src.database import save_releases_bulk, get_group_state, update_live_cursor, update_backfill_cursor

BACKFILL_SIZE = 5000


class Indexer:
    def __init__(self, client, mode="dynamic", verbose=False):
        self.client = client
        self.mode = mode
        self.phase = "backfill"
        self.verbose = verbose
        self.idle = False
        self.backfilling = False

    def index_group(self, group):
        count, first, last, name = self.client.select_group(group)

        state = get_group_state(group)

        if state is None:
            live_cursor = int(last)
            backfill_cursor = int(last)

            update_live_cursor(group, live_cursor)
            update_backfill_cursor(group, backfill_cursor)

            state = {
                "live_cursor": live_cursor,
                "backfill_cursor":backfill_cursor,
            }
            self.phase = "backfill"

        elif int(last) < state["live_cursor"]:
            state["live_cursor"] = int(last)
            state["backfill_cursor"] = int(last)

            update_live_cursor(group,int(last))
            update_backfill_cursor(group, int(last))
            self.phase = "backfill"

        if self.mode == "live":
            self.live(group, state, int(last))

        elif self.mode == "backfill":
            self.backfill(group, state, int(first), int(last))

        elif self.phase == "live":
            self.live(group, state, int(last))
            
        else:
            self.backfill(group, state, int(first), int(last))

    def live(self, group, state, last):
        start = state["live_cursor"] + 1

        if start > last:
            if self.verbose:
                print("no new articles")

            if not self.idle and not self.backfilling:
                print("[LIVE] no new articles, idle")
                self.idle = True
                
            self.mode = "backfill"
            return

        end = min(last, start + BACKFILL_SIZE - 1)
        self.process_range(group, start, end, "LIVE")
        update_live_cursor(group, end)
        self.mode = "backfill"

    def backfill(self, group, state, first, last):
        end = state["backfill_cursor"]

        if end > last:
            end = last

        if end < first:
            self.backfilling = False
            if not self.idle:
                print(f"[BACKFILL] {end} < first {first}, nothing to backfill, idle")
                self.idle = True
            self.phase = "live"
            self.mode = "live"
            return

        start = max(first, end - BACKFILL_SIZE + 1)
        self.process_range(group, start, end, "BACKFILL")
        update_backfill_cursor(group, start - 1)
        self.backfilling = True
        self.mode = "live"

    def process_range(self, group, start, end, kind):
        try:
            headers = list(self.client.fetch_headers(start, end))
        
        except nntp.NNTPTemporaryError as e:
            if e.code != 423:
                raise
            print(f"[{kind}] {start}-{end} empty, skipping")
            return
        
        except nntp.NNTPPermanentError as e:
            print(f"[{kind}] {start}-{end} not available ({e.code}), skipping")
            return

        self.idle = False

        if self.verbose:
            print(f"[{kind}] {len(headers)} headers")

        articles = headers_to_articles(headers)
        releases = group_articles(articles)

        to_save = []

        for release in releases.values():
            release["complete"] = is_complete(release)
            release["group"] = group
            release["poster"] = release["articles"][0].author
            release["date"] = release["articles"][0].date
            to_save.append(release)

        save_releases_bulk(to_save)
