import sqlite3
from src.database import database, with_db


def fts_query(query):
    terms = []

    for raw in query.split():
        term = raw.strip('"').replace('"', "")

        if term:
            terms.append(f'"{term}"*')

    return " AND ".join(terms)


def like_query(query):
    return f"%{query.strip()}%"


#try fts first, fall back to like if it chokes
@with_db
def _fts_or_like(conn, fts_sql, like_sql, fts_vals, like_vals, fetch_one = False):
    cur = conn.cursor()

    try:
        cur.execute(fts_sql, fts_vals)
    except sqlite3.OperationalError:
        cur.execute(like_sql, like_vals)

    if fetch_one:
        result = cur.fetchone()
    else:
        result = cur.fetchall()

    return result


def search_releases(query, group, page = 0, page_size = 10):
    if not query.strip():
        return []

    #page is 0 based
    offset = page * page_size

    #fts table holds text, real data lives in releases
    return _fts_or_like("""
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ? and r.group_name = ?
        order by r.name
        limit ? offset ?
    """, """
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        where r.name like ? and r.group_name = ?
        order by r.name
        limit ? offset ?
    """, (fts_query(query), group, page_size, offset), (like_query(query), group, page_size, offset))


def search_all_releases(query, page = 0, page_size = 10):
    if not query.strip():
        return []

    offset = page * page_size
    return _fts_or_like("""
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ?
        order by r.name
        limit ? offset ?
    """, """
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        where r.name like ?
        order by r.name
        limit ? offset ?
    """, (fts_query(query), page_size, offset), (like_query(query), page_size, offset))


def count_releases(query, group):
    if not query.strip():
        return 0

    #same fts query but just counting
    row = _fts_or_like("""
        select count(*) from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ? and r.group_name = ?
    """, """
        select count(*) from releases r
        where r.name like ? and r.group_name = ?
    """, (fts_query(query), group), (like_query(query), group), fetch_one = True)

    return row[0]


def count_all_releases(query):
    if not query.strip():
        return 0

    #same again all groups
    row = _fts_or_like("""
        select count(*) from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ?
    """, """
        select count(*) from releases r
        where r.name like ?
    """, (fts_query(query),), (like_query(query),), fetch_one = True)

    return row[0]


@with_db
def get_release(conn, id):
    cur = conn.cursor()

    cur.execute("""
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        where r.id = ?
    """, (id,)
    )

    release = cur.fetchone()
    return release


@with_db
def get_articles(conn, release_id):
    cur = conn.cursor()

    #keeps the parts of each file in order soo they can be reassembled
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
    return articles
