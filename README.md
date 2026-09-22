<div align="center">

# Atlas

**A self hosted Usenet indexer that lives in your terminal.**

Atlas is a Usenet indexer that indexes releases from NNTP newsgroups and stores them locally in SQLite. It comes with features like AI-powered search, a live dashboard, direct NZB downloads through SABnzbd, and more.


[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Python](https://img.shields.io/badge/python-3.10%2B-blue)
![Platform](https://img.shields.io/badge/platform-Arch%20Linux%20(x86__64)-informational)
![Docker](https://img.shields.io/badge/docker-supported-2496ED?logo=docker&logoColor=white)
![Hackatime](https://hackatime.hackclub.com/api/v1/badge/U09JP15EVQU/Eraxty/Atlas)

[Features](#features) • [Install](#installation) • [Usage](#usage) • [Docker](#docker) • [FAQ](#faq)

![Atlas](img/main.png)

</div>

---

## Why Atlas

Most Usenet indexers are either paid services or heavyweight self-hosted stacks built primarily around automation. Atlas is designed to work either way: use it directly from the terminal when you want to search and grab something yourself, or plug it into an automated *arr stack through its Newznab API.

Atlas handles NNTP indexing, release parsing, local SQLite storage, AI-powered search, NZB generation, and SABnzbd integration, while also supporting automation through Prowlarr and other Newznab compatible tools.

It gives you a self hosted indexer that works just as well for an interactive terminal workflow as it does as part of a fully automated Usenet setup.

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
| **Docker support** | Compose stack — Atlas + SABnzbd + Prowlarr on one network |

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

### Fresh Setup (compose stack)

The compose file brings up **Atlas + SABnzbd + Prowlarr** on the same Docker network, so they talk to each other by service name,no host IPs. Atlas talks to SABnzbd at `sabnzbd:8080` because it shares the SABnzbd config volume.

1. Fill in your provider credentials in `docker_compose.yml`:
   - `ATLAS_NNTP_HOST`, `ATLAS_NNTP_USER`, `ATLAS_NNTP_PASS`

2. Bring it all up:

   ```bash
   docker compose -f docker_compose.yml up -d
   ```

3. Grab the Atlas API key (generated on first run, can also find in config.json):

   ```bash
   docker compose -f docker_compose.yml logs atlas | grep "api key"
   ```

4. Point Prowlarr at Atlas:
   - Open Prowlarr at `http://localhost:9696`
   - **Indexers → Add Indexer → Newznab**
   - Name: `Atlas`
   - URL: `http://atlas:9090` (service name, same network)
   - API Path: `/api`
   - API Key: the key from step 3
   - Category: `Other` (7000)
   - **Test** — should come back green — then **Save**

Atlas is exposed on `http://localhost:9090` for Prowlarr-on-the-host setups too.

#### Verifying everything came up

**1. Containers running:**

```bash
docker compose -f docker_compose.yml ps
```

All three should read `Up` (sabnzbd, prowlarr, atlas).

**2. Atlas API is live:**

```bash
curl http://localhost:9090/api?t=caps
```

You should get a `<caps>` XML back with the server info. `t=caps` is the only endpoint without auth, so a `200` here means the API is up.

**3. Services can reach each other by name** (this is the whole point of the shared network):

```bash
# prowlarr -> atlas
docker exec prowlarr sh -c 'wget -q -S -O /dev/null "http://atlas:9090/api?t=caps" 2>&1 | grep -m1 HTTP/'

# atlas -> sabnzbd
docker exec atlas python -c "import urllib.request as u; print(u.urlopen('http://sabnzbd:8080', timeout=5).status)"
```

A `HTTP/1.1 200 OK` from the first and a `403` from the second are both correct — `403` is just SABnzbd's own API auth responding while the connection itself is fine.

**4. Get the Atlas API key** (generated on first run, also written to config):

```bash
docker compose -f docker_compose.yml exec atlas grep api_key /app/data/config.json
```

**5. Search with auth** — the API now needs the key:

```bash
KEY=$(docker compose -f docker_compose.yml exec -T atlas grep api_key /app/data/config.json | cut -d'"' -f4)
curl "http://localhost:9090/api?t=search&apikey=$KEY"
```

Expect an `<rss>` response with `<items>`. No key, or a wrong one, returns a `401` newznab error.

**6. Prowlarr's test** — in the Prowlarr UI, the indexer Test should go green with `Indexer added successfully`.

Stuck on step 2? Leave the NNTP creds empty and Atlas drops into its interactive setup wizard instead of starting the API — fill in `ATLAS_NNTP_USER` / `ATLAS_NNTP_PASS` in the compose file, then `docker compose up -d --force-recreate atlas`.

Stop everything with:

```bash
docker compose -f docker_compose.yml down
```

Rebuild after code changes with:

```bash
docker compose -f docker_compose.yml up -d --build
```

### Existing arr stack (just add the Atlas indexer)

Already running SABnzbd and Prowlarr? You don't need the full stack, run Atlas alone and point it at your existing services.

1. Run only Atlas:

   ```bash
   docker build -t atlas .
   docker run -d --name atlas \
     -e ATLAS_NNTP_HOST=news.usenet.farm \
     -e ATLAS_NNTP_USER=youruser \
     -e ATLAS_NNTP_PASS=yourpass \
     -e ATLAS_SAB_HOST=172.17.0.1 \
     -e ATLAS_SAB_PORT=8080 \
     -p 9090:9090 \
     -v atlas-data:/app/data \
     atlas
   ```

   `ATLAS_SAB_HOST` is wherever your SABnzbd lives — `172.17.0.1` if it runs on the host, `host.docker.internal` on Docker Desktop, or your SABnzbd container's service name.

2. Get the API key from the logs:

   ```bash
   docker logs atlas | grep "api key"
   ```

3. Add Atlas to your existing Prowlarr as a **Newznab** indexer:
   - Name: `Atlas`
   - URL: `http://<host-or-ip>:9090`
   - API Path: `/api`
   - API Key: the key from step 2
   - Category: `Other` (7000)
   - **Test**, then **Save**

### Environment variables

Set these in `docker_compose.yml` (fresh setup) or on `docker run` (existing stack):

| Variable | Description |
|---|---|
| `ATLAS_NNTP_HOST` | Your provider's server |
| `ATLAS_NNTP_PORT` | Default `563` |
| `ATLAS_NNTP_USER` | Your username |
| `ATLAS_NNTP_PASS` | Your password |
| `ATLAS_INDEX_MODE` | `dynamic` / `live` / `backfill` |
| `ATLAS_API_PORT` | Port Atlas's newznab API listens on — default `9090` |
| `ATLAS_SAB_HOST` | Hostname of your SABnzbd (`sabnzbd` in the compose stack) |
| `ATLAS_SAB_PORT` | SABnzbd's port — default `8080` |

### Backing up the database

The database lives in a Docker volume. Make sure the compose service is running, then:

```bash
docker compose -f docker_compose.yml exec atlas cp /app/data/atlas.db /app/atlas.db
docker cp atlas:/app/atlas.db ./backup.db
```
## FAQ

<details>
<summary>AI search isn't working</summary>

Make sure [Ollama](https://ollama.com) is installed and running locally, with a compatible model pulled (`ollama pull qwen3:4b`). Atlas doesn't ship with Ollama — it calls the local Ollama API.

</details>


## Platform

Tested on **Arch Linux, x86_64**. Other Linux distros may work but aren't officially verified.

## Limitation
No deobfuscation support yet

## Credits

Built by [Me](https://github.com/Eraxty) — 50+ days and 80+ hours of work, and my largest project to date. Special thanks to the Hack Club community for the push to build something like this.

**AI was used for:** bug fixes, refactoring, SABnzbd integration, the background indexer, terminal UI/dashboard polish and assistance.

## License

[GPL-3.0](LICENSE). Bundled SABnzbd is GPL-2.0-or-later and remains under its own license.
