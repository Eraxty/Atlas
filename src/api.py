import socket
from flask import Flask, Response, request
from threading import Thread
from werkzeug.serving import make_server
from email.utils import format_datetime, parsedate_to_datetime
from datetime import datetime, timezone
from xml.sax.saxutils import escape
from rich.console import Console

from src.search import search_all_releases, get_release, all_releases
from src.nzb import build_nzb, nzb_filename
from src.config import get_api_key


app = Flask(__name__)

console = Console()

api_key = None
api_thread = None
wsgi_server = None


def _pub_date(value):
    try:
        return format_datetime(parsedate_to_datetime(value))
    except (TypeError, ValueError):
        pass

    try:
        return format_datetime(datetime.fromisoformat(value).replace(tzinfo = timezone.utc))

    except (TypeError, ValueError):
        return format_datetime(datetime.now(timezone.utc))


@app.get("/api")
def api():
    t = request.args.get("t")

    if t == "caps":
        xml = """<?xml version="1.0" encoding="UTF-8"?>
<caps>
  <server version="1.0" title="Atlas" strapline="Atlas Usenet Indexer" url="http://127.0.0.1:9090" email="" image=""/>
  <limits max="100" default="100"/>
  <retention days="0"/>
  <registration available="no" open="no"/>
  <searching>
    <search available="yes" supportedParams="q,limit,offset"/>
    <tv-search available="no" supportedParams="q,season,ep"/>
    <movie-search available="no" supportedParams="q"/>
    <audio-search available="no" supportedParams="q"/>
  </searching>
  <categories>
    <category id="7000" name="Other"/>
  </categories>
</caps>"""

        return Response(xml, mimetype = "application/xml")

    if request.args.get("apikey") != api_key:
        return Response(
            '<?xml version="1.0" encoding="UTF-8"?>\n<error code="100" description="Invalid API Key"/>',
            mimetype = "application/xml",
            status = 401
        )

    if t == "search":
        q = request.args.get("q", "")

        try:
            limit = int(request.args.get("limit", 100))
        except (TypeError, ValueError):
            limit = 100

        try:
            offset = int(request.args.get("offset", 0))
        except (TypeError, ValueError):
            offset = 0

        limit = min(max(limit, 1), 100)
        offset = max(offset, 0)

        if q.strip():
            releases = search_all_releases(q, page = offset // limit, page_size = limit)
        else:
            releases = all_releases(page = offset // limit, page_size = limit)

        base = request.url_root.rstrip("/")

        items = []

        for r in releases:
            nzb_url = f"{base}/api?t=get&id={r[0]}&apikey={api_key}"
            items.append(
                "    <item>"
                f"      <title>{escape(r[1] or '')}</title>"
                f'      <guid isPermaLink="false">{r[0]}</guid>'
                f"      <link>{escape(nzb_url)}</link>"
                f"      <size>{r[5] or 0}</size>"
                f"      <pubDate>{_pub_date(r[4])}</pubDate>"
                f'      <enclosure url="{escape(nzb_url)}" type="application/x-nzb" length="{r[5] or 0}"/>'
                f'      <newznab:attr name="category" value="7000"/>'
                "    </item>"
            )

        xml = f"""<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"
     xmlns:atom="http://www.w3.org/2005/Atom"
     xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
    <channel>
        <title>Atlas</title>
        <description>Atlas search results</description>
        <link>{escape(base)}/api</link>
        <language>en-gb</language>
        <newznab:response offset="{offset}" total="{offset + len(releases)}"/>
        {"".join(items)}
    </channel>
</rss>"""

        return Response(xml, mimetype = "application/xml")

    if t == "get":
        try:
            release_id = int(request.args.get("id", 0))
        except (TypeError, ValueError):
            release_id = 0

        release = get_release(release_id)

        if release is None:
            return Response("", status = 404)

        content = build_nzb(release_id)

        if content is None:
            return Response("", status = 404)

        filename = nzb_filename(release[1], release_id)

        response = Response(content, mimetype = "application/x-nzb")
        response.headers["Content-Disposition"] = f'attachment; filename="{filename}"'

        return response

    return Response("", status = 404)


def start(config):
    global api_key, wsgi_server, api_thread

    api_key, created = get_api_key(config)

    if created:
        console.print(f"[yellow]api key: {api_key}[/yellow]")

    port = int(config.get("api_port", 9090))
    host = config.get("api_host", "127.0.0.1")

    if not _port_free(host, port):
        console.print(f"[red]port {port} is already in use, api not started[/red]")
        console.print("[dim]change it under Settings -> Change api port[/dim]")
        return None

    console.print(f"[dim]newznab api on http://{host}:{port} — apikey required for search/get[/dim]")

    wsgi_server = make_server(host, port, app, threaded = True)
    api_thread = Thread(target = wsgi_server.serve_forever, daemon = True)
    api_thread.start()

    return api_thread


def _port_free(host, port):
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind((host, port))
    except OSError:
        return False

    return True


def stop():
    global wsgi_server, api_thread

    if wsgi_server is not None:
        wsgi_server.shutdown()
        wsgi_server.server_close()

    wsgi_server = None
    api_thread = None