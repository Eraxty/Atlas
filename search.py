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
        select articles.message_id, articles.filename,
        articles.part, articles.total_parts, articles.bytes,
        articles.subject, releases.poster, releases.posted_date
        from articles join releases
        on articles.release_id = releases.id
        where articles.release_id = ?
        order by articles.part
    """, (release_id,))

    articles = cur.fetchall()
    conn.close()

    return articles

#0 message_id
#1 filename
#2 part
#3 total_parts
#4 bytes
#5 subject
#6 poster
#7 posted_date