from src.search import get_release, get_articles
from src.colors import red, green, reset
import time
import xml.etree.ElementTree as et
from datetime import datetime
from email.utils import parsedate_to_datetime
from pathlib import Path


def _article_timestamp(date_str):
    try:
        return int(parsedate_to_datetime(date_str).timestamp())
    except (TypeError, ValueError, OverflowError):
        pass

    try:
        return int(datetime.fromisoformat(date_str).timestamp())
    
    except (TypeError, ValueError):
        return int(time.time())


def generate_nzb(release_id, output_dir=None):
    release = get_release(release_id)
    articles = get_articles(release_id)

    if release is None:
        print(f"{red}Release not found{reset}")
        return

    if not articles:
        print(f"{red}No articles found{reset}")
        return

    #root tag
    nzb = et.Element("nzb", {"xmlns": "http://www.newzbin.com/DTD/2003/nzb"})

    #optional metadata
    head = et.SubElement(nzb, "head")
    meta = et.SubElement(head, "meta", {"type": "category"})
    meta.text = release[2] or ""

    #group articles by their actual filename, already sorted filename then part
    by_filename = {}
    
    for article in articles:
        by_filename.setdefault(article[1], []).append(article)

    #one <file> element per physical file, not per release
    for files_articles in by_filename.values():
        first = files_articles[0]
        timestamp = _article_timestamp(first[7])

        file = et.SubElement(
            nzb,
            "file",
            poster=first[6] or "",
            date=str(timestamp),
            subject=first[5] or ""
        )

        groups = et.SubElement(file, "groups")
        group = et.SubElement(groups, "group")
        group.text = release[2]

        segments = et.SubElement(file, "segments")

        for article in files_articles:
            segment = et.SubElement(
                segments,
                "segment",
                bytes=str(article[4]),
                number=str(article[2])
            )

            #nzb doesnt want the <>
            segment.text = article[0].strip("<>")

    et.indent(nzb, space="  ")

    safe_name = release[1].replace("/", "_")
    filename = f"{safe_name}.nzb"

    if output_dir:
        filename = str(Path(output_dir) / filename)

    #elementtree cant write doctype, so build the body ourselves
    body = et.tostring(nzb, encoding="unicode")

    try:
        with open(filename, "w", encoding="utf-8") as f:
            f.write(
                '<?xml version="1.0" encoding="utf-8"?>\n'
                '<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" '
                '"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">\n'
                + body
            )
    except OSError as e:
        print(f"{red}couldnt save nzb: {e}{reset}")
        return

    print(f"{green}Saved {filename}{reset}")


'''
release tuple
0 id
1 name
2 group_name
3 poster
4 posted_date
5 size
6 complete

article tuple
0 message_id
1 filename
2 part
3 total_parts
4 bytes
5 subject
6 author
7 posted_date
'''