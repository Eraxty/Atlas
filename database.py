import os
import sqlite3

database = "atlas.db"


def create_db():
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

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
            conn = sqlite3.connect(database)
            cursor = conn.cursor()

    cursor.execute("""
        create table if not exists releases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT UNIQUE,
            group_name TEXT,
            poster TEXT,
            posted_date TEXT,
            size INTEGER,
            complete INTEGER
        )
    """)

    cursor.execute("""
        create table if not exists articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            release_id INTEGER,
            message_id text unique,
            filename text,
            part INTEGER,
            total_parts INTEGER,
            bytes INTEGER,
            foreign key (release_id) references releases(id)
        )
    """)

    cursor.execute('''
        create table if not exists groups(
        name text primary key,
        last_article INTEGER
        )
    ''')

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
        insert or replace into releases
        (name, size, complete, group_name, poster, posted_date)
        values (?, ?, ?, ?, ?, ?)
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
        where name = ?
    """, (release["name"],))

    release_id = cursor.fetchone()[0]

    for article in release["articles"]:
        cursor.execute("""
            insert or replace into articles
            (release_id, message_id, filename, part, total_parts, bytes)
            values (?, ?, ?, ?, ?, ?)
        """, (
            release_id,
            article.message_id,
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


def get_last_article(group):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute('''
        select last_article 
        from groups
        where name = ?
    ''', (group,)
    )

    result = cursor.fetchone()
    conn.close()

    if result is None:
        return None

    return result[0]


def update_last_article(group, article):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        insert or replace into groups
        (name, last_article)
        values (?, ?)
    """, (group, article)
    )

    conn.commit()
    conn.close()
