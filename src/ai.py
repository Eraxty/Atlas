from src.config import load_config
from src.ai_chat import ask_ai
from src.ai_fetch import fetch_releases, fetch_and_store, fmt_size
from src.search import search_all_releases, count_all_releases, get_articles, recent_in_groups
from src.download import download_release
from src.nzb import generate_nzb
from src.prompts import prompt

from rich.console import Console
from rich.table import Table

console = Console()


def ai_search(config):
    import math, shutil, os

    while True:
        os.system("cls" if os.name == "nt" else "clear")
        
        print("AI Search")
        print("tell me what u want\n")

        query = prompt("> ").strip()
        
        if not query or query == "0":
            return

        print("\nthinking...\n")

        try:
            plan = ask_ai(query)
        except Exception as e:
            print(f"ai error: {e}")
            prompt("[enter]")
            continue

        print(plan.get("message", ""))
        print()

        groups = plan.get("groups", [])
        keywords = plan.get("keywords", [])

        if not groups:
            print("ai couldnt pick groups")
            prompt("[enter]")
            continue

        term = " ".join(keywords) if keywords else query

        # check local db first
        total = count_all_releases(term)
        print(f"found {total} in local db")
        saved = 0
        vague = False

        if total == 0:
            print(f"fetching from {len(groups)} groups...")
            saved = fetch_and_store(config, groups, keywords)
            
            print(f"saved {saved} releases\n")
            total = count_all_releases(term)

            if not total and saved:
                vague = True    
                total = saved

        if not total:
            print("nothing found")
            prompt("[enter]")
            continue

        page = 0

        while True:
            if vague:
                releases = recent_in_groups(groups, page, 15)
            else:
                releases = search_all_releases(term, page, 15)

            total_pages = max(1, math.ceil(total / 15))

            if page > total_pages - 1:
                page = total_pages - 1
                continue

            table = Table(title = f"{total} results for '{term}'")
            table.add_column("#", width = 4, justify = "right")
            table.add_column("Name", ratio = 3)
            table.add_column("Size", width = 10, justify = "right")
            table.add_column("Group", ratio = 1)

            for i, r in enumerate(releases, 1):
                table.add_row(str(i), r[1][:60], fmt_size(r[5]), r[2])

            console.print(table)
            print(f"[page {page + 1}/{total_pages}]")
            print("0. Back   n. Next   p. Prev   g. Goto page")

            choice = prompt("\nChoice: ").strip()

            if choice == "p":
                if page > 0:
                    page -= 1
                else:
                    print("already on the first page")
                    prompt("[enter]")
                continue

            if choice == "n":
                if page < total_pages - 1:
                    page += 1
                else:
                    print("already on the last page")
                    prompt("[enter]")
                continue

            if choice == "g":
                goto = prompt(f"page (1-{total_pages}): ")

                try:
                    target = int(goto)
                except ValueError:
                    target = -1

                if 1 <= target <= total_pages:
                    page = target - 1
                else:
                    print(f"page must be between 1 and {total_pages}")
                    prompt("[enter]")
                continue

            if choice == "0" or not choice:
                break

            try:
                selected = int(choice)
            except ValueError:
                print("invalid")
                prompt("[enter]")
                continue

            if selected < 1 or selected > len(releases):
                print("not on this page")
                prompt("[enter]")
                continue

            release = releases[selected - 1]
            rid = release[0]

            while True:
                print(f"\n{release[1][:60]}")
                print("1. Download")
                print("2. Save NZB")
                print("0. Back")

                act = prompt("\nChoice: ").strip()

                if act == "1":
                    try:
                        ok = download_release(rid)
                    except Exception as e:
                        print(f"couldnt queue: {e}")
                        ok = False
                    if ok:
                        print("download queued")
                    prompt("[enter]")
                    break


                if act == "2":
                    try:
                        generate_nzb(rid)
                    except Exception as e:
                        print(f"couldnt save nzb: {e}")
                    prompt("[enter]")
                    break


                if act == "0":
                    break

                print("invalid")
                prompt("[enter]")


def main():
    config = load_config()

    if not config:
        print("no config found, run main.py first")
        return

    print("\nAtlas AI Indexer")
    print("tell me what you want to find\n")

    query = input("> ").strip()

    if not query:
        return

    print("\nthinking...")

    try:
        plan = ask_ai(query)
    except Exception as e:
        print(f"ai failed: {e}")
        return

    print(f"\n{plan.get('message', '')}\n")

    results = fetch_releases(
        config,
        plan.get("groups", []),
        plan.get("keywords", []),
    )

    if not results:
        print("nothing found")
        return

    releases = {}

    for r in results:
        key = r["name"]
        if key not in releases:
            releases[key] = {"info": r, "parts": 0, "groups": set()}
        releases[key]["parts"] += 1
        releases[key]["groups"].add(r["group"])

    table = Table(title = f"{len(releases)} releases found")

    table.add_column("#", width = 4, justify = "right")
    table.add_column("Name", ratio = 3)
    table.add_column("Size", width = 10, justify = "right")
    table.add_column("Parts", width = 8, justify = "right")
    table.add_column("Group", ratio = 1)

    for i, (name, data) in enumerate(releases.items(), 1):
        info = data["info"]
        grp = ", ".join(data["groups"])

        table.add_row(
            str(i),
            name[:60],
            fmt_size(info["size"]),
            str(data["parts"]),
            grp,
        )

    console.print(table)


if __name__ == "__main__":
    main()