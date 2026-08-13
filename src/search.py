import sqlite3
from src.database import database


def fts_query(query):
    terms = []

    for raw in query.split():
        term = raw.strip('"')

        if term:
            terms.append(f'"{term}"*')

    return " AND ".join(terms)


def search_releases(query, group, page=0, page_size=10):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    offset = page * page_size

    try:
        cur.execute("""
            select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete,
            count(a.id)
            from releases r
            left join articles a on a.release_id = r.id
            join releases_fts on releases_fts.rowid = r.id
            where releases_fts match ? and r.group_name = ?
            group by r.id
            order by r.name
            limit ? offset ?
        """, (fts_query(query), group, page_size, offset)
        )
    except sqlite3.OperationalError:
        cur.execute("""
            select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete,
            count(a.id)
            from releases r
            left join articles a on a.release_id = r.id
            where r.name like ? and r.group_name = ?
            group by r.id
            order by r.name
            limit ? offset ?
        """, (f"%{query}%", group, page_size, offset)
        )

    releases = cur.fetchall()
    conn.close()

    return releases


def search_all_releases(query, page=0, page_size=10):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    offset = page * page_size

    try:
        cur.execute("""
            select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete,
            count(a.id)
            from releases r
            left join articles a on a.release_id = r.id
            join releases_fts on releases_fts.rowid = r.id
            where releases_fts match ?
            group by r.id
            order by r.name
            limit ? offset ?
        """,(fts_query(query), page_size, offset))
    except sqlite3.OperationalError:
        cur.execute("""
            select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete,
            count(a.id)
            from releases r
            left join articles a on a.release_id = r.id
            where r.name like ?
            group by r.id
            order by r.name
            limit ? offset ?
        """,(f"%{query}%", page_size, offset))

    releases = cur.fetchall()
    conn.close()

    return releases


def count_releases(query, group):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    try:
        cur.execute("""
            select count(*) from releases r
            join releases_fts on releases_fts.rowid = r.id
            where releases_fts match ? and r.group_name = ?
        """, (fts_query(query), group))
    except sqlite3.OperationalError:
        cur.execute("""
            select count(*) from releases r
            where r.name like ? and r.group_name = ?
        """, (f"%{query}%", group))

    count = cur.fetchone()[0]
    conn.close()

    return count


def count_all_releases(query):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    try:
        cur.execute("""
            select count(*) from releases r
            join releases_fts on releases_fts.rowid = r.id
            where releases_fts match ?
        """, (fts_query(query),))
    except sqlite3.OperationalError:
        cur.execute("""
            select count(*) from releases r
            where r.name like ?
        """, (f"%{query}%",))

    count = cur.fetchone()[0]
    conn.close()

    return count


def get_release(id):
    conn = sqlite3.connect(database, timeout=30)
    cur = conn.cursor()

    cur.execute("""
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete,
        count(a.id)
        from releases r
        left join articles a on a.release_id = r.id
        where r.id = ?
        group by r.id
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