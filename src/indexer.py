import nntp

from src.mapper import headers_to_articles
from src.parser import group_articles, is_complete
from src.database import save_releases_bulk, get_group_state, init_group_state, update_live_cursor, update_backfill_cursor

BACKFILL_SIZE = 5000


class Indexer:
    def __init__(self, client, mode="dynamic", verbose=False):
        self.client = client
        self.mode = mode
        self.verbose = verbose
        self.state = {}

    #per group phase/idle/backfilling
    def _gs(self, group):
        return self.state.setdefault(group, {
            "phase": "backfill",
            "idle": False,
            "backfilling": False,
        })

    def is_idle(self, group):
        return self._gs(group)["idle"]

    def is_backfilling(self, group):
        return self._gs(group)["backfilling"]

    def all_idle(self, groups):
        return not groups or all(self.is_idle(g) for g in groups)

    def index_group(self, group):
        count, first, last, name = self.client.select_group(group)

        state = get_group_state(group)

        if state is None:
            #both cursors start at the top
            live_cursor = int(last)
            backfill_cursor = int(last)

            init_group_state(group, int(last))

            state = {
                "live_cursor": live_cursor,
                "backfill_cursor":backfill_cursor,
            }
            self._gs(group)["phase"] = "backfill"

        #server renumbered, group got reset so start over
        elif int(last) < state["live_cursor"]:
            state["live_cursor"] = int(last)
            state["backfill_cursor"] = int(last)

            init_group_state(group, int(last))
            self._gs(group)["phase"] = "backfill"

        if self.mode == "live":
            self.live(group, state, int(last))

        elif self.mode == "backfill":
            self.backfill(group, state, int(first), int(last))

        elif self._gs(group)["phase"] == "live":
            self.live(group, state, int(last))
            
        else:
            self.backfill(group, state, int(first), int(last))

    def live(self, group, state, last):
        st = self._gs(group)
        st["backfilling"] = False
        start = state["live_cursor"] + 1

        #nothing new since last check
        if start > last:
            if self.verbose:
                print("no new articles")

            if not st["idle"]:
                print(f"[LIVE] {group} no new articles, idle")
                st["idle"] = True

            return

        end = min(last, start + BACKFILL_SIZE - 1)
        self.process_range(group, start, end, "LIVE")
        update_live_cursor(group, end)

    def backfill(self, group, state, first, last):
        st = self._gs(group)
        end = state["backfill_cursor"]

        if end > last:
            end = last

        if end < first:
            st["backfilling"] = False
            if not st["idle"]:
                print(f"[BACKFILL] {group} {end} < first {first}, nothing to backfill, idle")
                st["idle"] = True
            st["phase"] = "live"
            return

        #grab a chunk going backwards from the cur
        start = max(first, end - BACKFILL_SIZE + 1)
        self.process_range(group, start, end, "BACKFILL")
        update_backfill_cursor(group, start - 1)
        st["backfilling"] = True

    def process_range(self, group, start, end, kind):
        try:
            headers = list(self.client.fetch_headers(start, end))
        
        except nntp.NNTPTemporaryError as e:
            #423 = range has no articles
            if e.code != 423:
                raise
            print(f"[{kind}] {start}-{end} empty, skipping")
            return
        
        except nntp.NNTPPermanentError as e:
            print(f"[{kind}] {start}-{end} not available ({e.code}), skipping")
            return

        self._gs(group)["idle"] = False

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
