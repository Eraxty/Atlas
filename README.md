<div align="center">

# Atlas

**A self hosted Usenet indexer that lives in your terminal.**

Atlas is a Usenet indexer that indexes releases from NNTP newsgroups and stores them locally in SQLite. It comes with features like AI-powered search, a live dashboard, direct NZB downloads through SABnzbd, and more.


[![License: WTFPL](https://img.shields.io/badge/license-WTFPL-blue.svg)](LICENSE)
![Python](https://img.shields.io/badge/python-3.10%2B-blue)
![Platform](https://img.shields.io/badge/platform-Arch%20Linux%20(x86__64)-informational)
![Docker](https://img.shields.io/badge/docker-supported-2496ED?logo=docker&logoColor=white)
![Hackatime](https://hackatime.hackclub.com/api/v1/badge/U09JP15EVQU/Eraxty/Atlas)

[Features](#features) • [Install](#installation) • [Usage](#usage) • [Docker](#docker) • [FAQ](#faq)

![Atlas](img/main.png)

</div>

---

## Why Atlas

Most Usenet indexers are either a paid service or a heavyweight self hosted stack (\*arr style) built for automation pipelines, not for someone who just wants to search and grab something from the terminal. Atlas is the middle ground: a single Python app, a local SQLite database, and a search you can describe in plain English.

## Features

| | |
|---|---|
| **NNTP indexing** | Connects over SSL, rotates through all your groups automatically |
| **Dynamic indexing** | Switch between backfill only, live only, or dynamic mode |
| **Release parsing** | Handles multiple subject formats, flags complete vs. broken releases |
| **AI search** | Describe what you want in plain words — Atlas picks the groups and keywords itself |
| **Live dashboard** | Real time stats, throughput graphs, and group status in terminal |
| **NZB generation** | Generates NZB 1.1 files locally, no third party service |
| **SABnzbd integration** | Bundled SABnzbd 5.0.4, auto configured, opens in browser on download |
| **Background indexing** | Runs independently of the UI, start/stop without closing Atlas |
| **Local database** | Groups, releases, articles, and indexing state all in `atlas.db` |
| **Docker support** | Compose file included if you'd rather not manage a venv |

![Atlas dashboard](img/dash.png)

## Installation

**Requirements**
- Python 3.10+
- A Usenet provider account (NNTP, SSL enabled)

```bash
git clone https://github.com/Eraxty/Atlas
cd Atlas
python -m venv .venv
source .venv/bin/activate
pip install -r requirements.txt
python main.py
```

Prefer containers? Skip to [Docker](#docker).

## Usage

![Setup](img/login.png)

### First time setup

On first run, Atlas asks for your provider credentials:

| Field | What to enter |
|---|---|
| **Host** | Your provider's NNTP server, domain only — e.g. `news.usenet.farm` |
| **Username** | Your provider username |
| **Password** | Your provider password |
| **Port** | `563` (SSL) — leave as default |

Your password is stored in your OS keyring when possible. If keyring isn't available it falls back to `config.json`.

### Selecting groups

**Groups → search → add.** Text only groups are filtered out by default, and empty groups never show up.

### Indexing

Start the indexer from the main menu. Atlas begins pulling headers for every group you've selected. It also fires up the bundled SABnzbd in the background so downloads are ready when you want them. Pick a mode depending on what you need:

| Mode | Behavior |
|---|---|
| `dynamic` | Alternates backfill and live passes — keeps up with new posts while building history |
| `backfill` | Indexes backward from the latest release only |
| `live` | Indexes forward from the latest release only — nothing older |

### Searching

Two search scopes are available:

- **Current group** — searches only the group you're in
- **All groups** — searches everything you've indexed

**AI search** lets you describe what you want in plain language (`find me 4k hdr movies`) and Atlas figures out the groups and keywords, fetching anything missing from your database. Requires [Ollama](https://ollama.com) running locally with a model `qwen3:4b`. Speed depends on your hardware. 

### Downloading

Select a release and choose to generate an NZB or download directly. Downloading will:

1. Start SABnzbd if it isn't already running
2. Generate an NZB for the selected release
3. Drop it into SABnzbd's watched folder
4. Open SABnzbd in your browser so you can watch progress

Finished files land in `~/Downloads/complete`.

### Settings

- **Change config** — edit server credentials or groups without a full reset
- **Change indexer mode** — same three modes as above
- **Purge broken releases** — deletes incomplete releases, frees space
- **Wipe DB and cache** — full reset: database, logs, status, stats (stop the indexer first)

## Docker

### With Docker Compose

Fill in your credentials in `docker_compose.yml`, then:

```bash
docker compose -f docker_compose.yml up -d
docker compose -f docker_compose.yml exec atlas bash
```

Stop with:

```bash
docker compose -f docker_compose.yml down
```

### Environment variables

Set these in `docker_compose.yml` instead of using config files:

| Variable | Description |
|---|---|
| `ATLAS_NNTP_HOST` | Your provider's server |
| `ATLAS_NNTP_PORT` | Default `563` |
| `ATLAS_NNTP_USER` | Your username |
| `ATLAS_NNTP_PASS` | Your password |
| `ATLAS_INDEX_MODE` | `dynamic` / `live` / `backfill` |

### Without Compose

```bash
docker build -t atlas .
docker run -it --rm \
  -e ATLAS_NNTP_HOST=news.usenet.farm \
  -e ATLAS_NNTP_USER=youruser \
  -e ATLAS_NNTP_PASS=yourpass \
  -v atlas-data:/app/data \
  atlas
```

### Backing up the database

The database lives in a Docker volume. Make sure the compose service is running, then:

```bash
docker compose -f docker_compose.yml exec atlas cp /app/data/atlas.db /app/atlas.db
docker cp atlas:/app/atlas.db ./backup.db
```

### SABnzbd in Docker

The bundled SABnzbd only ships with the normal install, not the container image. In Docker you can still search, index, and save NZBs — but to download, point your own SABnzbd at the NZB files instead.

## FAQ

<details>
<summary>AI search isn't working</summary>

Make sure [Ollama](https://ollama.com) is installed and running locally, with a compatible model pulled (`ollama pull qwen3:4b`). Atlas doesn't ship with Ollama — it calls the local Ollama API.
<details>

## Limitation
No deobfuscation support yet

## Platform

Tested on **Arch Linux, x86_64**. Other Linux distros may work but aren't officially verified.

## Credits

Built by [Me](https://github.com/Eraxty) — 50+ days and 80+ hours of work, and my largest project to date. Special thanks to the Hack Club community for the push to build something like this.

**AI was used for:** bug fixes, refactoring, SABnzbd integration, the background indexer, terminal UI/dashboard polish and assistance.

## License
idrc jus give me credit ig 
[WTFPL](LICENSE)
