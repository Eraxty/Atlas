<div align="center">

# Atlas

A self hosted Usenet indexer that lives in your terminal

[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)
![Python](https://img.shields.io/badge/python-3.10%2B-blue)
![Platform](https://img.shields.io/badge/platform-Arch%20Linux%20(x86__64)-informational)
![Docker](https://img.shields.io/badge/docker-supported-2496ED?logo=docker&logoColor=white)
![Hackatime](https://hackatime.hackclub.com/api/v1/badge/U09JP15EVQU/Eraxty/Atlas)

[Features](#features) • [Install](#installation) • [Usage](#usage) • [Docker](#docker) • [Newznab API](#newznab-api-generic) • [Backup](#backing-up-the-database)

![Atlas](img/main.png)

</div>

---

## Why Atlas

I built Atlas because I wanted to make a Usenet indexer. A lot of indexers today are paid and expensive, Meanwhile atlas is opensource and free. Atlas keeps the useful parts in one place, it reads your provider, works out which posts belong together, stores them locally and makes an NZB or Download directly depending on your needs when you find something.

You can use it in the terminal. If you already run Prowlarr, SABnzbd, or the *arr apps, its Newznab API gets into that setup too.

## Features

- Index selected NNTP groups over SSL in live, backfill, or dynamic modes.
- Parse subjects into releases and mark incomplete sets.
- Search usenet through  local AI using Ollama.
- Watch indexing progress in the terminal.
- Generate NZBs and send downloads to the bundled SABnzbd.
- Keeps the data in a local SQLite database.

![Atlas dashboard](img/dash.png)

## Installation

You need:

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

### Binaries

It also got pre built binaries on the [release page](https://github.com/Eraxty/Atlas/releases).

#### Linux

1. Download `atlas-linux`.
2. Make it executable and run it:

```bash
chmod +x atlas-linux
./atlas-linux
```

#### Windows

1. Download `atlas-windows.zip`.
2. Extract it. It contains both `atlas-windows.exe` and `atlas.bat`.
3. Double click `atlas.bat`. It opens a terminal and starts Atlas.
4. 

If it doesnt work run it manually from a terminal

```powershell
cd atlas-windows
.\atlas-windows.exe
```

#### macOS

1. Download `atlas-macos`.
2. Make it executable and run it:

```bash
chmod +x atlas-macos
./atlas-macos
```

## Usage

![Setup](img/login.png)

### First time setup

On first run, Atlas asks for your provider credentials:

| Field | What to enter |
|---|---|
| Host | Your provider's NNTP server. Use the domain only, e.g. `news.yourprovider.net` |
| Username | Your provider username |
| Password | Your provider password |
| Port | `563` for SSL. Leave the default as it is |

Your password is stored in your OS keyring when possible. If keyring isn't available it falls back to `config.json`.

### Selecting groups

Go to Groups -> search -> add. Text groups and empty groups are filtered out by default.

### Indexing

> Note: indexing reads headers from the Usenet source server. It does not scan your computer.

Start the indexer from the main menu. Atlas starts pulling headers for every group you've selected. It also starts up the SABnzbd in the background so downloads are ready when you want them.You can also pick a mode for indexing bassed on your needs:-

| Mode | Behavior |
|---|---|
| `dynamic` | Alternates backfill and live passes, keeping up with new posts while building history |
| `backfill` | Indexes backward from the latest release only |
| `live` | Indexes forward from the latest release only and ignores older posts |

### Searching

Two search options are available :-

- Current group: search only the group you are in
- All groups: search everything you have indexed

AI search lets you describe what you want in plain language like (`find me 4k hdr movies`). The dumb AI selects the groups and keywords, then fetches anything missing from your database. It needs [Ollama](https://ollama.com) running locally, `qwen3:4b` model, Also speed of AI depends on your hardware because its local.

### Downloading

Select a release and chooseif u want to download NZB or download directly. Downloading directly will:

1. Start SABnzbd if it isn't already running
2. Generate an NZB for the selected release
3. Drop it into SABnzbd's watched folder
4. Opens SABnzbd in browser

Finished files are in `~/Downloads/complete`.

### Settings

- Change config: edit server credentials or groups without a full reset
- Change indexer mode: switch between same three modes told earlier
- Purge broken releases: delete incomplete releases to free up space
- Wipe DB and cache: clear the database, logs, status, and stats (stop the indexer first)
- Change API port: move the Newznab API off `9090` if that port is taken

## Docker

### Prebuilt image

No need to build locally just pull and run :-

```bash
docker pull ghcr.io/eraxty/atlas:latest
```

```bash
docker run -dit --name atlas \
  -e ATLAS_NNTP_HOST=news.your-provider.net \
  -e ATLAS_NNTP_USER=youruser \
  -e ATLAS_NNTP_PASS=yourpass \
  -e ATLAS_API_HOST=0.0.0.0 \
  -p 9090:9090 \
  -v atlas-data:/app/data \
  ghcr.io/eraxty/atlas:latest
```

needs `-it` because Atlas is a terminal app, and `ATLAS_API_HOST=0.0.0.0` lets the API be reached from outside the container.

Check if it's running :-

```bash
curl http://localhost:9090/api?t=caps
```

### Fresh Setup (compose stack)

The compose file runs Atlas, SABnzbd, and Prowlarr on same Docker network. They use service names instead of host IPs. Atlas reaches SABnzbd at `sabnzbd:8080` because they share the SABnzbd config volume.

1. Fill in your provider credentials in `docker_compose.yml`:
   - `ATLAS_NNTP_HOST`, `ATLAS_NNTP_USER`, `ATLAS_NNTP_PASS`

   That file is tracked by git, so don't commit your real password into it.

2. Bring it up:

   ```bash
   docker compose -f docker_compose.yml up -d
   ```

3. Grab the Atlas API key (generated on first run, then stored in the `atlas-data` volume so it survives restarts, and also written to config.json):

   ```bash
   docker compose -f docker_compose.yml logs atlas | grep "api key"
   ```

4. Point Prowlarr at Atlas:
   - Open Prowlarr at `http://localhost:9696`
   - Go to Indexers then Add Indexer then Newznab
   - Name: `Atlas`
   - URL: `http://atlas:9090` (service name, same network)
   - API Path: `/api`
   - API Key: the key from step 3
   - Category: `Other` (7000)
   - Run Test. It should be green, then save the indexer

Atlas is exposed on `http://localhost:9090` for Prowlarr on the host setups too.

#### Verifying the stack

Run these two checks:

```bash
docker compose -f docker_compose.yml ps
curl http://localhost:9090/api?t=caps
```

The first should show Atlas, Sabnzbd, and Prowlarr as `Up`. The second should return `<caps>` XML. This endpoint does not need an API key.

Atlas opens its setup wizard when the NNTP credentials are empty. Fill in `ATLAS_NNTP_USER` and `ATLAS_NNTP_PASS` in the compose file, then run
```bash
docker compose -f docker_compose.yml up -d --force-recreate atlas
```

Stop everything with:

```bash
docker compose -f docker_compose.yml down
```

Rebuild after code changes with:

```bash
docker compose -f docker_compose.yml up -d --build
```

### Existing arr stack (just add the indexer)

If you are already running SABnzbd and Prowlarr, You don't need the full stack, run Atlas and point it at your existing services.

1. Run only Atlas (swap `docker build` + `atlas` for `docker pull ghcr.io/eraxty/atlas:latest` if you don't want to build):

   ```bash
   docker build -t atlas .
   docker run -d --name atlas \
     -e ATLAS_NNTP_HOST=news.your-provider.net \
     -e ATLAS_NNTP_USER=youruser \
     -e ATLAS_NNTP_PASS=yourpass \
     -e ATLAS_SAB_HOST=172.17.0.1 \
     -e ATLAS_SAB_PORT=8080 \
     -e ATLAS_API_HOST=0.0.0.0 \
     -p 9090:9090 \
     -v atlas-data:/app/data \
     atlas
   ```

   `ATLAS_SAB_HOST` points to your SABnzbd. Use `172.17.0.1` when it runs on the host, host.docker.internal on Docker Desktop or the service name of its container. `ATLAS_API_HOST=0.0.0.0` is required so the published port can reach the API without it, Atlas defaults to `127.0.0.1` and stays on localhost.

2. Get the API key from the logs:

   ```bash
   docker logs atlas | grep "api key"
   ```

3. Add Atlas to your existing Prowlarr as a Newznab indexer:
   - Name: `Atlas`
   - URL: `http://<host-or-ip>:9090`
   - API Path: `/api`
   - API Key: the key from step 2
   - Category: `Other` (7000)
   - Run Test, then save

### Environment variables

Set these in `docker_compose.yml` (fresh setup) or on `docker run` (existing stack):

| Variable | Description |
|---|---|
| `ATLAS_NNTP_HOST` | Your provider's server |
| `ATLAS_NNTP_PORT` | Default `563` |
| `ATLAS_NNTP_USER` | Your username |
| `ATLAS_NNTP_PASS` | Your password |
| `ATLAS_INDEX_MODE` | `dynamic` / `live` / `backfill` |
| `ATLAS_API_PORT` | Port for Atlas's Newznab API Defaults to `9090` |
| `ATLAS_API_HOST` | Interface the API binds to. Defaults to `127.0.0.1` (localhost only). Set to `0.0.0.0` to accept connections from other hosts or containers. The compose stack sets this so Prowlarr can reach Atlas over the Docker network |
| `ATLAS_SAB_HOST` | Hostname of your SABnzbd (`sabnzbd` in the compose stack) |
| `ATLAS_SAB_PORT` | SABnzbd's port. Defaults to `8080` |
| `ATLAS_HOME` | Directory holding `config.json`, `atlas.db`, and logs the Docker image sets this to `/app/data` |

## Newznab API (Generic)

Atlas exposes a generic Newznab compatible API, the protocol used by Prowlarr, Sonarr, Radarr, and SABnzbd. Any compatible client can search releases and fetch NZB's.

- Endpoint: `http://<host>:<port>/api`. The port defaults to `9090`.
- Binding: the API listens on `127.0.0.1` by default. Set `ATLAS_API_HOST=0.0.0.0` to expose it to other machines or containers.
- Authentication: `t=caps` works without a key. Every other operation needs the `apikey` parameter. A missing or wrong key returns a `401` Newznab error.

### Operations

| `t=` | Meaning | Auth |
|---|---|---|
| `caps` | Capability discovery (server info, supported params, categories) | No |
| `search` | Release search. `q` is optional; an empty query returns recent releases | Yes |
| `get` | Download the NZB for a release by `id` | Yes |

### Parameters

| Param | Applies to | Description |
|---|---|---|
| `apikey` | all (except `caps`) | Your API key |
| `q` | `search` | Plain word search terms matched against release names |
| `cat` | `search` | Accepted for compatibility; Atlas currently indexes category `7000` (Other) only |
| `limit` | `search` | Max results, default `100`, clamped to `100` |
| `offset` | `search` | Result offset for pagination |
| `id` | `get` | Release ID from a search result's `<guid>` |

### Examples

```bash
# capabilities (no auth)
curl "http://localhost:9090/api?t=caps"

# search releases, key required
KEY=$(jq -r .api_key config.json)
curl "http://localhost:9090/api?t=search&apikey=$KEY&q=matrix&limit=25"

# download the NZB for a specific release
curl -O "http://localhost:9090/api?t=get&id=1234&apikey=$KEY"
```

Under Docker, `config.json` lives in the `atlas-data` volume, so read the key from there instead of the host:

```bash
KEY=$(docker compose -f docker_compose.yml exec -T atlas jq -r .api_key /app/data/config.json)
```

### Capabilities

`t=caps` advertises search (`q`, `limit`, `offset`, with up to 100 results), one category (`7000`, Other), and no registration. That is why Prowlarr should use category `Other (7000)` during setup.

### Search results

Each `<item>` carries Newznab-compatible metadata:

- `<title>`: release name
- `<guid>`: release ID, used with `t=get`
- `<link>` / `<enclosure>`: NZB download URL
- `<size>`: total size in bytes
- `<pubDate>`: posted date in RFC 2822 format
- `<newznab:attr name="category" value="7000"/>`: category

Prowlarr reads these to evaluate hits and hands the `<enclosure>` URL (Atlas's `t=get` endpoint) to the downloader. `t=get` responds with `application/x-nzb` and a `Content-Disposition` attachment header containing a valid NZB 1.1 file built from the indexed articles, so SABnzbd can grab it straight off the URL.

## Backing up the database

Under Docker, the database lives in a volume. Make sure the compose service is running then :-

```bash
docker compose -f docker_compose.yml exec atlas cp /app/data/atlas.db /app/atlas.db
docker cp atlas:/app/atlas.db ./backup.db
```

Running from source :-

```bash
cp atlas.db ./backup.db
```

## Platform

Tested on Arch Linux, x86_64. Other distros may work but aren't officially verified.

## Limitation

Partial deobfuscation support via par2
Windows exe and Mac build might not work properly

## Credits

Built by [Me](https://github.com/Eraxty) over 60 days and 100+ hours. It is my biggest project so far Thanks to the Hack Club community for pushing me to build something like this.

AI helped with bug fixes, refactoring, SABnzbd integration, the background indexer, and terminal UI polish, Docker setup ,Testing and few more small things.

## License

[GPL-3.0](LICENSE). Bundled SABnzbd is GPL-2.0 or later and remains under its own license.
