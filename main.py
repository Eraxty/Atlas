from dataclasses import dataclass
from parser import group_articles, is_complete
from database import create_db, save_release, get_releases

@dataclass
class Article:
    number: int
    subject: str
    author: str
    date: str
    message_id: str
    references: str
    bytes: int
    lines: int


articles = [
    Article(
        number=12345601,
        subject='"ahh.smthin.part01.rar" yEnc (1/3)',
        author="narendramodi@gmail.com",
        date="23 Jul 2026 12:30:00 GMT",
        message_id="<smthing@example.com>",
        references="",
        bytes=750000,
        lines=10000
    )
]

releases = group_articles(articles)

for name, release in releases.items():
    print(f"Release: {name}")
    print(f"Size: {release['size']:,} bytes")
    print(f"Complete: {is_complete(release)}")
    print("Articles:")

    for article in release["articles"]:
        print(
            f"Part {article['part']}/{article['total_parts']} "
            f"{article['message_id']}"
        )

create_db()

for release in releases.values():
    save_release(
        release["name"],
        release["size"],
        is_complete(release)
    )

for release in get_releases():
    print(release)