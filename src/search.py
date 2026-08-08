import sqlite3
from src.database import database


def search_releases(query, group, page=0, page_size=10):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    offset = page * page_size

    cur.execute("""
        select id, name, group_name, poster, posted_date, size, complete
        from releases
        where group_name = ? and name like ?
        order by name
        limit ? offset ?
    """, (group, f"%{query}%", page_size, offset)
    )

    releases = cur.fetchall()
    conn.close()

    return releases


def search_all_releases(query, page=0, page_size=10):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    offset = page * page_size

    cur.execute("""
        select id, name from releases where name like ?
        order by name
        limit ? offset ?
    """,(f"%{query}%", page_size, offset))

    releases = cur.fetchall()
    conn.close()

    return releases


def count_releases(query, group):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    cur.execute("""
        select count(*) from releases
        where group_name = ? and name like ?
    """, (group, f"%{query}%"))

    count = cur.fetchone()[0]
    conn.close()

    return count


def count_all_releases(query):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    cur.execute("""
        select count(*) from releases
        where name like ?
    """, (f"%{query}%",))

    count = cur.fetchone()[0]
    conn.close()

    return count


def get_release(id):
    conn = sqlite3.connect(database, timeout=30)
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
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    cur.execute("""
        select articles.message_id, articles.filename,
        articles.part, articles.total_parts, articles.bytes,
        articles.subject, releases.poster, releases.posted_date
        from articles join releases
        on articles.release_id = releases.id
        where articles.release_id = ?
        order by articles.filename, articles.part 
    """, (release_id,))

    articles = cur.fetchall()
    conn.close()

    return articles