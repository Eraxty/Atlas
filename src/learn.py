import json
import os
from pathlib import Path

BASE_DIR = Path(__file__).resolve().parent.parent
PROFILE_FILE = BASE_DIR / "profile.json"


def load():
    try:
        with open(PROFILE_FILE) as f:
            return json.load(f)
    except (OSError, json.JSONDecodeError):
        return {"terms": {}, "searches": 0, "downloads": 0}


def save(profile):
    tmp = PROFILE_FILE.with_suffix(".json.tmp")

    with open(tmp, "w") as f:
        json.dump(profile, f)

    os.replace(tmp, PROFILE_FILE)


def terms(text):
    out = set()

    for raw in text.lower().split():
        word = "".join(c for c in raw if c.isalnum())

        if len(word) > 2:
            out.add(word)

    return out


def learn(text, kind = "search"):
    profile = load()

    if kind == "download":
        profile["downloads"] += 1
        weight = 3.0
    else:
        profile["searches"] += 1
        weight = 1.0

    for word in terms(text):
        current = profile["terms"].get(word, 0)
        profile["terms"][word] = current * 0.9 + weight

    save(profile)


def score(text):
    profile = load()["terms"]
    return sum(profile.get(word, 0) for word in terms(text))


def top(n = 15):
    profile = load()["terms"]
    return sorted(profile.items(), key = lambda kv: kv[1], reverse = True)[:n]