#THIS FILE IS VIBE CODED do not blame me for bugs 

from pathlib import Path
import fnmatch
import getpass
import json
import math
import nntplib
import os
import shutil
import time
import urllib.request

from src.colors import reset, dim, red, green, yellow, cyan
from src.search import count_all_releases, search_all_releases, get_articles
from src.download import download_release
from src.nzb import generate_nzb
from src.prompts import prompt

BASE_DIR = Path(__file__).resolve().parent.parent
STATUS_FILE = BASE_DIR / "status.json"


def clear():
    os.system("cls" if os.name == "nt" else "clear")


def fmt_size(size):
    if size is None:
        return "?"
    if size <= 0:
        return "0 B"
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if size < 1024:
            return f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} PB"


def fmt_date(value):
    if not value:
        return ""
    return str(value)[:10]

OLLAMA_URL = "http://localhost:11434/api/generate"
OLLAMA_MODEL = "qwen3:1.7b"


def _read_status():
    try:
        with open(STATUS_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}


def _fetch_groups(config):
    password = config.get("password") or getpass.getpass("Password: ")
    try:
        conn = nntplib.NNTP_SSL(
            config["host"],
            port = config["port"],
            user = config["username"],
            password = password,
        )
    except (nntplib.NNTPError, OSError, TimeoutError) as e:
        print(f"{red}couldnt connect to server: {e}{reset}")
        return []

    try:
        conn._putcmd("LIST ACTIVE")
        resp, lines = conn._getlongresp()
        return [line.split()[0].decode() if isinstance(line.split()[0], bytes) else line.split()[0] for line in lines if line.strip()]
    except (nntplib.NNTPError, OSError, TimeoutError) as e:
        print(f"{red}couldnt list groups: {e}{reset}")
        return []
    finally:
        conn.quit()


def _shortlist(query, groups, limit = 150):
    words = [w.lower() for w in query.split() if len(w) > 2]

    scored = []
    for g in groups:
        gl = g.lower()
        hits = sum(1 for w in words if w in gl)
        if "alt.binaries" in gl:
            hits += 1
        if hits:
            scored.append((hits, g))

    scored.sort(reverse = True)
    return [g for _, g in scored[:limit]] or fnmatch.filter(groups, "*binaries*")[:limit]


def _ask_llm_for_group(query, candidates):
    if not candidates:
        return None

    listing = "\n".join(candidates)
    body = {
        "model": OLLAMA_MODEL,
        "prompt": (
            "user wants to find this on usenet: \"" + query + "\"\n"
            "pick the single best newsgroup from this list for finding it. "
            "reply with ONLY the group name, nothing else.\n\n" + listing
        ),
        "stream": False,
    }

    try:
        req = urllib.request.Request(
            OLLAMA_URL,
            data = json.dumps(body).encode(),
            headers = {"Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout = 30) as resp:
            answer = json.loads(resp.read())["response"].strip()
    except Exception as e:
        print(f"{red}local llm call failed: {e}{reset}")
        return None

    answer = answer.strip("`\"' \n")

    if answer in candidates:
        return answer

    for c in candidates:
        if c.lower() in answer.lower():
            return c

    return None


def _show_release(release, articles):
    print(f"Release: {release[1]}")
    print(f"{dim}poster: {release[3] or 'unknown'}   posted: {fmt_date(release[4])}{reset}")
    status = f"{green}complete{reset}" if release[6] else f"{red}incomplete{reset}"
    print(f"{fmt_size(release[5])} - {release[7]} parts - {status}\n")
    if not articles:
        return
    files = {}
    for a in articles:
        files.setdefault(a[1] or "?", []).append(a)
    width = max(20, shutil.get_terminal_size().columns - 20)
    print(f"files ({len(files)}):")
    shown = list(files.items())[:30]
    for filename, parts in shown:
        present = len({p[2] for p in parts})
        expected = max((p[3] for p in parts if p[3]), default = present)
        name = filename if len(filename) <= width else filename[:width - 3] + "..."
        print(f"  {name}  {dim}{present}/{expected}{reset}")
    if len(files) > len(shown):
        print(f"  {dim}... and {len(files) - len(shown)} more{reset}")
    print()


def _show_results(releases, query, page, total_pages, total, page_size):
    clear()
    print(f"Search: {query}")
    print(f"Page {page + 1} of {total_pages}")
    start = page * page_size + 1
    end = min((page + 1) * page_size, total)
    print(f"{dim}Showing {start}-{end} of {total} results{reset}\n")
    width = max(20, shutil.get_terminal_size().columns - 40)
    for i, release in enumerate(releases, 1):
        name = release[1]
        if len(name) > width:
            name = name[:width - 3] + "..."
        broken = f"  {red}[broken]{reset}" if not release[6] else ""
        print(f"{i}. {name}  {fmt_size(release[5])} - {release[7]} parts - {fmt_date(release[4])}{broken}")
    print("\n0. Back")
    if page > 0:
        print("p. Previous Page")
    if page < total_pages - 1:
        print("n. Next Page")
    print("g. Go to Page")


def auto_mode(config, start_background_indexer, indexer_alive, do_search):
    query = input("What are you looking for: ").strip()

    if not query:
        return

    total = count_all_releases(query)

    if total:
        print(f"\n{green}found {total} matches already indexed{reset}")
        _show_paginated_results(config, query)
        return

    print(f"\n{yellow}nothing indexed for that yet{reset}")
    print("fetching group list off the server...")

    groups = _fetch_groups(config)
    if not groups:
        return

    candidates = _shortlist(query, groups)

    print(f"asking {OLLAMA_MODEL} to pick a group from {len(candidates)} candidates...")
    group = _ask_llm_for_group(query, candidates)

    if not group:
        print(f"{red}llm couldnt pick a group, try Groups menu manually{reset}")
        input("[enter]")
        return

    print(f"{cyan}picked: {group}{reset}")

    config["group"] = group

    if not indexer_alive():
        print(f"starting indexer on {group}...")
        if not start_background_indexer():
            print(f"{red}couldnt start indexer{reset}")
            input("[enter]")
            return

    print(f"{dim}indexing {group}, ctrl-c any time to check now{reset}\n")

    try:
        while True:
            status = _read_status()

            if status.get("idle"):
                break

            if not indexer_alive():
                break

            count = status.get("indexed_count", 0)
            print(f"\r{cyan}indexing...{reset} {count} releases so far   ", end = "", flush = True)
            time.sleep(1)

    except KeyboardInterrupt:
        pass

    print(f"\n{green}done{reset}")
    _show_paginated_results(config, query)


def _show_paginated_results(config, query):
    import math
    import shutil

    page = 0
    page_size = max(10, shutil.get_terminal_size().lines - 15)

    while True:
        total = count_all_releases(query)
        releases = search_all_releases(query, page, page_size)

        if not total:
            print(f"\n{red}no releases found{reset}")
            prompt("[enter]")
            return

        total_pages = max(1, math.ceil(total / page_size))

        if page > total_pages - 1:
            page = total_pages - 1
            continue

        _show_results(releases, query, page, total_pages, total, page_size)

        choice = input("\nChoice: ").strip()

        if choice == "0":
            return

        if choice == "p":
            if page > 0:
                page -= 1
            else:
                print(f"{dim}already on the first page{reset}")
                input("[enter]")
            continue

        if choice == "n":
            if page < total_pages - 1:
                page += 1
            else:
                print(f"{dim}already on the last page{reset}")
                input("[enter]")
            continue

        if choice == "g":
            goto = input(f"Go to page (1-{total_pages}): ")
            try:
                target = int(goto)
            except ValueError:
                target = -1
            if 1 <= target <= total_pages:
                page = target - 1
            else:
                print(f"{red}page must be between 1 and {total_pages}{reset}")
                input("[enter]")
            continue

        try:
            selected = int(choice)
        except ValueError:
            print(f"{red}invalid{reset}")
            input("[enter]")
            continue

        if selected < 1 or selected > len(releases):
            print(f"{red}not on this page{reset}")
            input("[enter]")
            continue

        release = releases[selected - 1]
        choice_id = release[0]
        articles = get_articles(choice_id)

        while True:
            clear()
            _show_release(release, articles)
            print("1. Download")
            print("2. Save NZB")
            print("0. Back")

            choice = input("\nChoice: ").strip()

            if choice == "1":
                try:
                    ok = download_release(choice_id)
                except Exception as e:
                    print(f"{red}couldnt queue download: {e}{reset}")
                    ok = False
                if ok:
                    print(f"\n{green}Download queued it is downloading in background.{reset}")
                    print(f"{dim}Finished files land ~/Downloads{reset}")
                input("[enter]")

            if choice == "2":
                try:
                    generate_nzb(choice_id)
                    print(f"\n{green}NZB saved{reset}")
                except Exception as e:
                    print(f"{red}couldnt save: {e}{reset}")
                input("[enter]")

            if choice == "0":
                break
