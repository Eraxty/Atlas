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

    conn.commit()
    conn.close()


def save_release(name, size, complete):
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        insert or replace into releases
        (name, size, complete)
        values (?, ?, ?)
    """, (name, size, int(complete)))

    conn.commit()
    conn.close()


def get_releases():
    conn = sqlite3.connect(database)
    cursor = conn.cursor()

    cursor.execute("""
        select id, name, size, complete
        from releases
        order by id
    """)

    releases = cursor.fetchall()
    conn.close()
    return releases

