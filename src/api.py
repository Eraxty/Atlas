from flask import Flask, Response, request
from threading import Thread
from email.utils import format_datetime, parsedate_to_datetime
from datetime import datetime, timezone
from xml.sax.saxutils import escape

from src.search import search_all_releases, get_release
from src.nzb import build_nzb, nzb_filename


app = Flask(__name__)


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
<caps xmlns="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <server title="Atlas" version="1.0" url="http://127.0.0.1:8080/api" />
  <limits max="100" default="100" />
</caps>"""

        return Response(xml, mimetype = "application/xml")

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

        releases = search_all_releases(q, page = offset // limit, page_size = limit)

        base = request.url_root.rstrip("/")

        items = []

        for r in releases:
            nzb_url = f"{base}/api?t=get&id={r[0]}"
            items.append(
                "    <item>"
                f"      <title>{escape(r[1] or '')}</title>"
                f'      <guid isPermaLink="false">{r[0]}</guid>'
                f"      <link>{escape(nzb_url)}</link>"
                f"      <size>{r[5] or 0}</size>"
                f"      <pubDate>{_pub_date(r[4])}</pubDate>"
                f'      <enclosure url="{escape(nzb_url)}" type="application/x-nzb" length="{r[5] or 0}"/>'
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
        <newznab:response offset="{offset}" total="{len(releases)}"/>
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
    port = int(config.get("api_port", 9090))
    
    thread = Thread(
        target = app.run,
        kwargs = {"host": "127.0.0.1", "port": port, "use_reloader": False},
        daemon = True,
    )
  
    thread.start()

    return thread