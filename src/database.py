import os
import sqlite3

database = "atlas.db"


def create_db():
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    #check releases schema
    cursor.execute("""
        select name from sqlite_master
        where type = 'table' and name = 'releases'
    """)
    if cursor.fetchone() is not None:
        cursor.execute("pragma table_info(releases)")
        release_columns = {column[1] for column in cursor.fetchall()}

        if not {"group_name", "poster", "posted_date"}.issubset(release_columns):
            conn.close()
            os.remove(database)
            return create_db()

    #check articles schema
    cursor.execute("""
        select name from sqlite_master
        where type = 'table' and name = 'articles'
    """)
    if cursor.fetchone() is not None:
        cursor.execute("pragma table_info(articles)")
        article_columns = {column[1] for column in cursor.fetchall()}

        if "subject" not in article_columns:
            conn.close()
            os.remove(database)
            return create_db()

    cursor.execute("""
        create table if not exists releases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT,
            group_name TEXT,
            poster TEXT,
            posted_date TEXT,
            size INTEGER,
            complete INTEGER
        )
    """)

    cursor.execute("""
        create unique index if not exists idx_release_unique
        on releases(name, group_name)
    """)

    cursor.execute("""
        create table if not exists articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            release_id INTEGER,
            message_id TEXT UNIQUE,
            subject TEXT,
            filename TEXT,
            part INTEGER,
            total_parts INTEGER,
            bytes INTEGER,
            foreign key (release_id) references releases(id)
        )
    """)

    cursor.execute("""
        create table if not exists groups(
            name TEXT PRIMARY KEY,
            live_cursor INTEGER,
            backfill_cursor INTEGER
        )
    """)

    cursor.execute("""
        create index if not exists idx_release_name
        on releases(name)
    """)

    cursor.execute("""
        create index if not exists idx_release_group
        on releases(group_name)
    """)

    cursor.execute("""
        create index if not exists idx_release_date
        on releases(posted_date)
    """)

    conn.commit()
    conn.close()


def save_release(release):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        insert into releases
        (name, size, complete, group_name, poster, posted_date)
        values (?, ?, ?, ?, ?, ?)
        on conflict(name, group_name) do update set
        size = excluded.size,
        complete = excluded.complete,
        poster = excluded.poster,
        posted_date = excluded.posted_date
    """, (
        release["name"],
        release["size"],
        int(release["complete"]),
        release["group"],
        release["poster"],
        release["date"]
    ))

    cursor.execute("""
        select id from releases
        where name = ? and group_name = ?
    """, (
        release["name"],
        release["group"]
    ))

    release_id = cursor.fetchone()[0]

    cursor.execute("delete from articles where release_id = ?", (release_id,))

    for article in release["articles"]:
        cursor.execute("""
            insert into articles
            (release_id, message_id, subject, filename, part, total_parts, bytes)
            values (?, ?, ?, ?, ?, ?, ?)
        """, (
            release_id,
            article.message_id,
            article.subject,
            article.filename,
            article.part,
            article.total_parts,
            article.bytes
        ))

    conn.commit()
    conn.close()


def get_releases():
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        select id, name, size, complete, group_name, poster, posted_date
        from releases
        order by posted_date desc
    """)

    releases = cursor.fetchall()
    conn.close()
    return releases


def get_group_state(group):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute('''
        select live_cursor , backfill_cursor
        from groups
        where name = ?
    ''', (group,)
    )

    result = cursor.fetchone()
    conn.close()

    if result is None:
        return None

    return {
        "live_cursor":result[0],
        "backfill_cursor":result[1],
    }


def update_live_cursor(group, article):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        insert into groups(name, live_cursor)
        values(?, ?)
        on conflict(name)
        do update set live_cursor = excluded.live_cursor
    """, (group, article)
    )

    conn.commit()
    conn.close()

def update_backfill_cursor(group,article):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        insert into groups(name, backfill_cursor)
        values(?, ?)
        on conflict(name)
        do update set backfill_cursor = excluded.backfill_cursor
    """, (group, article)
    )

    conn.commit()
    conn.close()
