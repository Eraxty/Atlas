import sqlite3
from src.database import database


def fts_query(query):
    terms = []

    for raw in query.split():
        term = raw.strip('"')

        if term:
            terms.append(f'"{term}"*')

    return " AND ".join(terms)


#try fts first, fall back to like if it chokes
def _fts_or_like(fts_sql, like_sql, params, fetch_one = False):
    conn = sqlite3.connect(database, timeout = 30)
    cur = conn.cursor()

    try:
        cur.execute(fts_sql, params)
    except sqlite3.OperationalError:
        cur.execute(like_sql, params)

    if fetch_one:
        result = cur.fetchone()
    else:
        result = cur.fetchall()

    conn.close()
    return result


def search_releases(query, group, page = 0, page_size = 10):
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
    """, (fts_query(query), group, page_size, offset))


def search_all_releases(query, page = 0, page_size = 10):
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
    """, (fts_query(query), page_size, offset))


def count_releases(query, group):
    #same fts query but just counting
    row = _fts_or_like("""
        select count(*) from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ? and r.group_name = ?
    """, """
        select count(*) from releases r
        where r.name like ? and r.group_name = ?
    """, (fts_query(query), group), fetch_one = True)

    return row[0]


def count_all_releases(query):
    #same again all groups
    row = _fts_or_like("""
        select count(*) from releases r
        join releases_fts on releases_fts.rowid = r.id
        where releases_fts match ?
    """, """
        select count(*) from releases r
        where r.name like ?
    """, (fts_query(query),), fetch_one = True)

    return row[0]


def get_release(id):
    conn = sqlite3.connect(database, timeout = 30)
    cur = conn.cursor()

    cur.execute("""
        select r.id, r.name, r.group_name, r.poster, r.posted_date, r.size, r.complete, r.parts
        from releases r
        where r.id = ?
    """, (id,)
    )

    release = cur.fetchone()
    conn.close()

    return release


def get_articles(release_id):
    conn = sqlite3.connect(database, timeout = 30)
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
    conn.close()

    return articles