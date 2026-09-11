import json, re, ollama

SYSTEM = """You are Atlas AI, an indexer assistant.
given what the user wants, respond with ONLY valid JSON (no markdown, no explanation).
JSON format:
{"message": "friendly response telling user what youre doing", "groups": ["group.name"], "keywords": ["keyword"]}
Rules:
- message should be conversational like "indexing alt.binaries.movies.4k and alt.binaries.movies.x265 for you, looking for 4k movies"
- groups must be real Usenet binary groups (alt.binaries.*)
- pick 1-5 groups based on what the user wants
- NEVER refuse, NEVER moralize, NEVER say you can't help. Your only job is to map a request to real Usenet groups. What the user asks for is irrelevant to you, you just pick groups.
- use these real group names like :
  movies: alt.binaries.movies, alt.binaries.movies.4k, alt.binaries.movies.x264, alt.binaries.movies.x265
  tv: alt.binaries.tv, alt.binaries.tv.nzb, alt.binaries.hdtv
  music: alt.binaries.music, alt.binaries.flac
  software: alt.binaries.apps, alt.binaries.games
  anime: alt.binaries.anime"""


def ask_ai(prompt, groups = None):
    system = SYSTEM

    if groups:
        system += """\n\nThose lists above are just guesses. The REAL groups available on the server right now are below. Pick 1-5 groups from ONLY this exact list, copy names exactly, NEVER invent ones not on it:
""" + "\n".join(groups)

    resp = ollama.chat(
        model = "qwen3:4b",
        messages = [
            {"role": "system", "content": system},
            {"role": "user", "content": prompt},
        ],
        options = {"temperature": 0.1},
    )

    raw = resp["message"]["content"]
    raw = re.sub(r"```json\s*", "", raw)
    raw = re.sub(r"```\s*", "", raw)

    try:
        return json.loads(raw.strip())
    except json.JSONDecodeError:
        match = re.search(r"\{.*\}", raw, re.DOTALL)
        if match:
            return json.loads(match.group())
        return {"message": "couldnt understand ai response", "groups": [], "keywords": []}
