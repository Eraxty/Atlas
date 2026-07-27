import sqlite3

database = "atlas.db"


def create_db():
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        create table if not exists releases (
            id integer primary key autoincrement,
            name text unique,
            size integer,
            complete integer
        )
    """)

    cursor.execute("""
        create table if not exists articles (
            id int primary key autoincrement,
            release_id int,
            message_id text unique,
            filename text,
            part int,
            total_parts int,
            bytes int,
            foreign key (release_id) references releases(id)
        )
    """)

    cursor.execute('''
        create table if not exists groups(
        name text primary key,
        last_article int
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