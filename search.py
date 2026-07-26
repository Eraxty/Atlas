import sqlite3
from database import database


def search_releases(query):
    conn = sqlite3.connect(database)
    cur = conn.cursor()

    cur.execute("""
        select id, name, size, complete
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
        select id, name, size, complete
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