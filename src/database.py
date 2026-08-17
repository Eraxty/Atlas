import sqlite3
from pathlib import Path
from functools import wraps

BASE_DIR = Path(__file__).resolve().parent.parent
database = BASE_DIR / "atlas.db"


def with_db(func):
    @wraps(func)
    def wrapper(*args, **kwargs):
        conn = sqlite3.connect(database, timeout=30)
        try:
            return func(conn, *args, **kwargs)
        finally:
            conn.close()
    return wrapper


def migrate(conn):
    #old db files are missing these columns, add em if they aint there
    cursor = conn.cursor()

    cursor.execute("pragma table_info(releases)")

    release_columns = {column[1] for column in cursor.fetchall()}

    if "group_name" not in release_columns:
        cursor.execute("alter table releases add column group_name TEXT")
    
    if "poster" not in release_columns:
        cursor.execute("alter table releases add column poster TEXT")
    
    if "posted_date" not in release_columns:
        cursor.execute("alter table releases add column posted_date TEXT")

    if "parts" not in release_columns:
        cursor.execute("alter table releases add column parts INTEGER")

        #backfill parts for releases already in the db
        cursor.execute("""
            update releases set parts = (
                select count(*) from articles where release_id = releases.id
            ) where parts is null
        """)

    cursor.execute("pragma table_info(articles)")
    
    article_columns = {column[1] for column in cursor.fetchall()}

    if "subject" not in article_columns:
        cursor.execute("alter table articles add column subject TEXT")


def create_db():
    conn = sqlite3.connect(database, timeout=30)
    cursor = conn.cursor()
    #wal soo the indexer can write while search reads
    cursor.execute("pragma journal_mode = wal")

    cursor.execute("""
        create table if not exists releases (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT,
            group_name TEXT,
            poster TEXT,
            posted_date TEXT,
            size INTEGER,
            complete INTEGER,
            parts INTEGER
        )
    """)

    #this is the key the upsert matches on, same release = same row
    cursor.execute("""
        create unique index if not exists idx_release_unique
        on releases(name, group_name)
    """)

    #one row per usenet message, unique message_id soo we never store the same message twice
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

    cursor.execute("""
        create index if not exists idx_articles_release
        on articles(release_id)
    """)

    migrate(conn)

    #external content fts only holds the name column, real data stays in releases
    fts_exists = cursor.execute("""
        select name from sqlite_master
        where type = 'table' and name = 'releases_fts'
    """).fetchone()

    if fts_exists is None:
        cursor.execute("""
            create virtual table releases_fts using fts5(
                name,
                content='releases',
                content_rowid='id'
            )
        """)

    #keep fts in sync with releases
    cursor.execute("""
        create trigger if not exists releases_ai after insert on releases begin
            insert into releases_fts(rowid, name) values (new.id, new.name);
        end
    """)

    cursor.execute("""
        create trigger if not exists releases_ad after delete on releases begin
            insert into releases_fts(releases_fts, rowid, name) values ('delete', old.id, old.name);
        end
    """)

    cursor.execute("""
        create trigger if not exists releases_au after update on releases begin
            insert into releases_fts(releases_fts, rowid, name) values ('delete', old.id, old.name);
            insert into releases_fts(rowid, name) values (new.id, new.name);
        end
    """)


    if fts_exists is None:
        #fill fts with whatever rows already exist
        cursor.execute("insert into releases_fts(releases_fts) values ('rebuild')")

    conn.commit()
    conn.close()


def _release_stats(cur, release_id):
    files = {}
    size = 0

    for filename, part, total, b in cur.execute("""
        select filename, part, total_parts, bytes
        from articles where release_id = ?
    """, (release_id,)):
        files.setdefault(filename, []).append((part, total))
        size += b

    part_count = sum(len(file_parts) for file_parts in files.values())

    for file_parts in files.values():
        expected = max(total for _, total in file_parts)

        if {part for part, _ in file_parts} != set(range(1, expected + 1)):
            return size, 0, part_count

    return size, 1, part_count


def save_releases_bulk(releases):
    
    if not releases:
        return

    conn = sqlite3.connect(database, timeout=30)

    #transaction soo a half written batch rolls back
    with conn:
        cur = conn.cursor()
        
        for release in releases:
            cur.execute("""
                insert into releases
                (name, size, complete, group_name, poster, posted_date)
                values (?, ?, ?, ?, ?, ?)
                on conflict(name, group_name) do update set
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

            cur.execute("""
                select id from releases 
                where name = ? and group_name = ?
            """, (release["name"], release["group"]))
                
            row = cur.fetchone()

            if row is None:
                continue

            release_id = row[0]

            #remembers the change count soo we can tell if this pass added anything
            before = conn.total_changes
            
            cur.executemany("""
                insert or ignore into articles
                (release_id, message_id, subject, 
                filename, part, total_parts, bytes)
                values (?, ?, ?, ?, ?, ?, ?)
            """, [
                (release_id,
                a.message_id, 
                a.subject, 
                a.filename, 
                a.part, 
                a.total_parts, 
                a.bytes)
                for a in release["articles"]
            ])

            #nothing new added so the stored stats are still same
            if conn.total_changes == before:
                continue

            size, complete, part_count = _release_stats(cur, release_id)
            cur.execute("""
                update releases set size = ?, complete = ?, parts = ?
                where id = ?
            """, (size, complete, part_count, release_id))

    conn.close()


@with_db
def get_releases(conn):
    cursor = conn.cursor()

    cursor.execute("""
        select id, name, size, complete, group_name, poster, posted_date
        from releases
        order by posted_date desc
    """)

    releases = cursor.fetchall()
    return releases


@with_db
def get_group_state(conn, group):
    cursor = conn.cursor()

    cursor.execute('''
        select live_cursor , backfill_cursor
        from groups
        where name = ?
    ''', (group,)
    )

    result = cursor.fetchone()

    if result is None:
        return None

    return {
        "live_cursor":result[0],
        "backfill_cursor":result[1],
    }


@with_db
def update_live_cursor(conn, group, article):
    cursor = conn.cursor()

    cursor.execute("""
        insert into groups(name, live_cursor)
        values(?, ?)
        on conflict(name)
        do update set live_cursor = excluded.live_cursor
    """, (group, article)
    )

    conn.commit()

@with_db
def update_backfill_cursor(conn, group, article):
    cursor = conn.cursor()

    cursor.execute("""
        insert into groups(name, backfill_cursor)
        values(?, ?)
        on conflict(name)
        do update set backfill_cursor = excluded.backfill_cursor
    """, (group, article)
    )

    conn.commit()