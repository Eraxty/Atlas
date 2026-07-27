from search import get_release, get_articles
import xml.etree.ElementTree as et

def generate_nzb(release_id):
    release = get_release(release_id)
    articles = get_articles(release_id)

    if release is None:
        print("Release not found")
        return

    nzb = et.Element("nzb")

    file = et.SubElement(nzb, "file", subject=release[1])

    groups = et.SubElement(file, "groups")
    group = et.SubElement(groups, "group")
    group.text = "alt.binaries.test"

    segments = et.SubElement(file, "segments")

    for article in articles:
        segment = et.SubElement(
            segments,
            "segment",
            bytes=str(article[4]),
            number=str(article[2])
        )

        segment.text = article[0]

    tree = et.ElementTree(nzb)
    tree.write(f"{release[1]}.nzb", encoding="utf-8", xml_declaration=True)

    print(f"Saved {release[1]}.nzb")