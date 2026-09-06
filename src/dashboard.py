import json
import time
from pathlib import Path
import plotext as plt
from rich.live import Live
from rich.panel import Panel
from rich.layout import Layout
from rich.text import Text
from rich.table import Table
from rich.progress_bar import ProgressBar
from rich.align import Align

STATS_FILE = Path(__file__).resolve().parent.parent / "stats.json"


def load():
    try:
        with open(STATS_FILE) as f:
            return json.load(f)
    except Exception:
        return {}


def human_bytes(n):
    for u in ["B", "KB", "MB", "GB", "TB"]:
        if n < 1024:
            return f"{n:.1f}{u}"
        n /= 1024
    return f"{n:.1f}PB"


def human_time(s):
    m, s = divmod(int(s), 60)
    h, m = divmod(m, 60)
    return f"{h}h{m:02d}m" if h else f"{m}m{s:02d}s"


def ago(t):
    d = time.time() - t
    return f"{int(d)}s ago" if d < 60 else f"{int(d / 60)}m ago"


def state_color(s):
    if not s.get("running"):
        return "red", "stopped"
    if s.get("error"):
        return "red", "error"
    if s.get("idle"):
        return "yellow", "idle"
    return "green", "running"


def status_panel(s, color, label):
    t = Text()
    t.append("●  ", style = color)
    t.append(f"{label}\n\n", style = f"bold {color}")
    t.append("mode    ", style = "dim")
    t.append(f"{s.get('mode', '?')}\n", style = "white")
    t.append("uptime  ", style = "dim")
    t.append(f"{human_time(s.get('uptime', 0))}\n", style = "white")
    t.append("errors  ", style = "dim")
    t.append(f"{s.get('error_count', 0)}", style = "white")
    return Panel(Align.left(t, vertical = "middle"), title = "status", border_style = "bright_black")


def totals_panel(s):
    t = Text()
    t.append("articles  ", style = "dim")
    t.append(f"{s.get('total_articles', 0)}\n", style = "cyan")
    t.append("releases  ", style = "dim")
    t.append(f"{s.get('total_releases', 0)}\n", style = "cyan")
    t.append("indexed   ", style = "dim")
    t.append(f"{human_bytes(s.get('total_bytes', 0))}\n", style = "cyan")
    t.append("db size   ", style = "dim")
    t.append(human_bytes(s.get('db_size', 0)), style = "cyan")
    return Panel(Align.left(t, vertical = "middle"), title = "totals", border_style = "bright_black")


def speed_graph(s, w, h):
    hist = s.get("history", [])
    bs = [x["b"] for x in hist] or [0]
    rate = [max(b2 - b1, 0) for b1, b2 in zip(bs, bs[1:])] or [0, 0]
    plt.clf()
    plt.plotsize(w, h)
    plt.theme("pro")
    plt.plot(rate, marker = "braille")
    plt.axes_color("default")
    plt.canvas_color("default")
    plt.ticks_color("default")
    return plt.build()


def speed_panel(s, w, h):
    graph = speed_graph(s, w - 4, h - 5)
    t = Text()
    t.append(f"avg {s.get('avg_byte_speed', 0):.1f} B/s   ", style = "dim")
    t.append(f"peak {s.get('peak_byte_speed', 0):.1f} B/s\n", style = "dim")
    t.append(Text.from_ansi(graph))
    return Panel(t, title = "throughput", border_style = "bright_black")


def groups_panel(s):
    groups = s.get("groups", {})
    max_art = max([g.get("articles", 0) for g in groups.values()] + [1])
    table = Table(box = None, expand = True, pad_edge = False)
    table.add_column("group", style = "cyan")
    table.add_column("articles", justify = "right")
    table.add_column("load", ratio = 1)
    table.add_column("last indexed", style = "dim", justify = "right")

    for name, g in groups.items():
        table.add_row(
            name,
            str(g.get("articles", 0)),
            ProgressBar(total = max_art, completed = g.get("articles", 0), width = None),
            ago(g.get("last_indexed", time.time())),
        )

    return Panel(table, title = f"groups ({s.get('groups_indexed', 0)})", border_style = "bright_black")


def render(w, h):
    s = load()
    color, label = state_color(s)

    layout = Layout()
    layout.split_column(
        Layout(name = "top", size = 8),
        Layout(name = "mid", size = 10),
        Layout(name = "bottom", ratio = 1),
    )
    layout["top"].split_row(
        Layout(status_panel(s, color, label)),
        Layout(totals_panel(s)),
    )
    layout["mid"].update(speed_panel(s, w, 10))
    layout["bottom"].update(groups_panel(s))

    footer = Text()
    footer.append("\n  Ctrl+C to go back\n", style = "dim")

    full = Layout()
    full.split_column(
        Layout(name = "dash", ratio = 1),
        Layout(name = "foot", size = 3),
    )
    full["dash"].update(layout)
    full["foot"].update(footer)

    return full


if __name__ == "__main__":
    with Live(render(80, 24), refresh_per_second = 2, screen = True) as live:
        while True:
            time.sleep(0.5)
            w, h = live.console.size
            live.update(render(w, h))
