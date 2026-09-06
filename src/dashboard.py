import json, time, collections
from rich.live import Live
from rich.panel import Panel
from rich.layout import Layout
from rich.text import Text
from rich.align import Align

STATUS_FILE = "status.json"
BLOCKS = " ▁▂▃▄▅▆▇█"

hist = collections.deque([0] * 40, maxlen = 40)

def load():
    try:
        with open(STATUS_FILE) as f:
            return json.load(f)
    except Exception:
        return {}

def sparkline(vals):
    lo, hi = min(vals), max(vals)
    span = hi - lo or 1
    
    return "".join(BLOCKS[int((v - lo) / span * (len(BLOCKS) - 1))] for v in vals)

def state_color(s):
    if s.get("error"):
        return "red", "error"
    
    if s.get("idle"):
        return "yellow", "idle"
    
    return "green", "running"
