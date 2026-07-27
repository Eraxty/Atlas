import sqlite3
from database import database


def search_releases(query):
    conn = sqlite3.connect(database)
    cur = conn.cursor()

    cur.execute("""
        select id, name, group_name, poster, posted_date, size, complete
        from releases
        where name like ?
        order by name
    """, (f"%{query}%",)
    )

    releases = cur.fetchall()
    conn.close()

    return releases


def get_release(id):
    conn = sqlite3.connect(database)
    cur = conn.cursor()

    cur.execute("""
        select id, name, group_name, poster, posted_date, size, complete
        from releases
        where id = ?
    """, (id,)
    )

    release = cur.fetchone()
    conn.close()

    return release


def get_articles(release_id):
    conn = sqlite3.connect(database)
    cur = conn.cursor()

    cur.execute("""
        select message_id, filename, part, total_parts, bytes
        from articles
        where release_id = ?
        order by part
    """, (release_id,)
    )

    articles = cur.fetchall()
    conn.close()

    return articles

# release tuple:
# 0=id, 1=name, 2=group_name, 3=poster,
# 4=posted_date, 5=size, 6=complete