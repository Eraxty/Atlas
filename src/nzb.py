from src.search import get_release, get_articles
import xml.etree.ElementTree as et
from email.utils import parsedate_to_datetime
from pathlib import Path

def generate_nzb(release_id, output_dir=None):
    release = get_release(release_id)
    articles = get_articles(release_id)

    if release is None:
        print("Release not found")
        return

    if not articles:
        print("No articles found")
        return

    #first article
    first = articles[0]

    #root tag
    nzb = et.Element("nzb", {"xmlns": "http://www.newzbin.com/DTD/2003/nzb"})

    #optional metadata
    head = et.SubElement(nzb, "head")
    meta = et.SubElement(head, "meta", {"type": "category"})
    meta.text = release[2]

    #nzb uses unix timestamps
    timestamp = int(parsedate_to_datetime(first[7]).timestamp())

    #release info
    file = et.SubElement(
        nzb,
        "file",
        poster=first[6],
        date=str(timestamp),
        subject=first[5]
    )

    #newsgroup
    groups = et.SubElement(file, "groups")
    group = et.SubElement(groups, "group")
    group.text = release[2]

    #all the message ids go here
    segments = et.SubElement(file, "segments")

    #already sorted
    for article in articles:
        segment = et.SubElement(
            segments,
            "segment",
            bytes=str(article[4]),
            number=str(article[2])
        )

        #nzb doesnt want the <>
        segment.text = article[0].strip("<>")

    tree = et.ElementTree(nzb)
    et.indent(tree, space="  ")

    safe_name = release[1].replace("/", "_")
    filename = f"{safe_name}.nzb"

    if output_dir:
        filename = str(Path(output_dir) / filename)

    #save xml
    tree.write(filename, encoding="utf-8", xml_declaration=True)

    #elementtree cant write doctype
    with open(filename, "r+", encoding="utf-8") as f:
        xml = f.read()
        f.seek(0)
        f.write(
            '<?xml version="1.0" encoding="utf-8"?>\n'
            '<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" '
            '"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">\n'
            + xml.split("\n", 1)[1]
        )
        f.truncate()

    print(f"Saved {filename}")


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