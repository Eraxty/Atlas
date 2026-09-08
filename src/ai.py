from src.config import load_config
from src.ai_chat import ask_ai
from src.ai_fetch import fetch_releases, fmt_size

from rich.console import Console
from rich.table import Table

console = Console()


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
