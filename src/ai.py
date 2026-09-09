from src.config import load_config
from src.ai_chat import ask_ai
from src.ai_fetch import fetch_releases, fetch_and_store, fmt_size
from src.search import search_all_releases, count_all_releases, get_articles
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

        print(f"fetching from {len(groups)} groups...")
        saved = fetch_and_store(config, groups, keywords)
        print(f"saved {saved} releases\n")

        term = " ".join(keywords) if keywords else query
        total = count_all_releases(term)
        releases = search_all_releases(term, 0, 20)

        if not total:
            print("nothing found")
            prompt("[enter]")
            continue

        table = Table(title = f"{total} results for '{term}'")
        table.add_column("#", width = 4, justify = "right")
        table.add_column("Name", ratio = 3)
        table.add_column("Size", width = 10, justify = "right")
        table.add_column("Group", ratio = 1)

        for i, r in enumerate(releases, 1):
            table.add_row(str(i), r[1][:60], fmt_size(r[5]), r[2])

        console.print(table)
        prompt("\n[enter]")


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
