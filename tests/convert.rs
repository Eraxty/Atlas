//! Converting a database from before the shards: every release and article
//! moves into its group's shard, NZBs and stats come out the same, cursors
//! carry over, the old file is kept aside, and saving afterwards continues
//! with ids after the old ones.

use std::path::Path;

use atlas::{convert, db, nzb, store};
use rusqlite::Connection;

/// A database in the old layout: everything in atlas.db.
fn old_database(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "
        pragma journal_mode = wal;
        create table releases (
            id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT, group_name TEXT, poster TEXT, posted_date TEXT,
            size INTEGER, complete INTEGER, parts INTEGER, file_total INTEGER, display_name TEXT,
            is_obfuscated INTEGER default 0
        );
        create table articles (
            id INTEGER PRIMARY KEY AUTOINCREMENT, release_id INTEGER, message_id TEXT, subject TEXT, filename TEXT,
            part INTEGER, total_parts INTEGER, bytes INTEGER, file_total INTEGER,
            unique(release_id, message_id)
        );
        create table groups(name TEXT PRIMARY KEY, live_cursor INTEGER, backfill_cursor INTEGER,
            first_article INTEGER, last_article INTEGER);
        insert into groups values ('alt.binaries.a@news.x', 900, 500, 1, 1000), ('alt.binaries.b', 70, 10, 5, 80);
        ",
    )
    .unwrap();

    let groups = ["alt.binaries.a", "alt.binaries.b", "alt.binaries.c", "alt.binaries.d"];
    let mut article = conn
        .prepare(
            "insert into articles (release_id, message_id, subject, filename, part, total_parts, bytes, file_total)
             values (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .unwrap();
    for r in 1..=40i64 {
        let group = groups[(r % 4) as usize];
        conn.execute(
            "insert into releases (id, name, group_name, poster, posted_date, size, complete, parts, file_total,
                display_name, is_obfuscated) values (?, ?, ?, 'poster <p@x>', '2026-10-02 10:11:12', ?, ?, ?, 2, ?, ?)",
            rusqlite::params![
                r * 3, // gaps in the ids, like a real database
                format!("Release.{r}"),
                group,
                r * 1000,
                r % 2,
                r % 7 + 1,
                (r % 5 == 0).then(|| format!("Real.Name.{r}")),
                (r % 3 == 0) as i64
            ],
        )
        .unwrap();
        for n in 1..=(r % 7 + 1) {
            let file = if n == 3 && r % 6 == 0 { None } else { Some(format!("file{}.rar", n % 2)) };
            // hex ids at a shared domain, odd ones, and two articles sharing a part number
            let message_id = match n {
                1 => format!("<{:032x}@ngPost>", r * 100 + n),
                2 => format!("<Odd-{r}-{n}@JBinUp.local>"),
                _ => format!("<x{r}y{n}@nyuu>"),
            };
            let part = if n == 4 {
                Some(1)
            } else if n == 5 {
                None
            } else {
                Some(n)
            };
            article
                .execute(rusqlite::params![
                    r * 3,
                    message_id,
                    format!("\"file{}.rar\" yEnc ({n}/7)", n % 2),
                    file,
                    part,
                    7,
                    100 + n,
                    2
                ])
                .unwrap();
        }
    }
    // an article whose release is gone
    article.execute(rusqlite::params![9999, "<orphan@x>", "s", "f", 1, 1, 1, 1]).unwrap();
}

/// every release's NZB from the old database, by release name
/// (name, nzb, (size, complete, parts))
type OldRelease = (String, String, (Option<i64>, bool, Option<i64>));

fn old_nzbs(path: &Path) -> Vec<OldRelease> {
    let conn = Connection::open(path).unwrap();
    let mut releases = conn
        .prepare("select id, name, group_name, poster, posted_date, size, complete, parts from releases order by id")
        .unwrap();
    let rows: Vec<atlas::search::ReleaseRow> = releases
        .query_map([], |r| {
            Ok(atlas::search::ReleaseRow {
                id: r.get(0)?,
                name: r.get(1)?,
                group_name: r.get(2)?,
                poster: r.get(3)?,
                posted_date: r.get(4)?,
                size: r.get(5)?,
                complete: r.get::<_, i64>(6)? != 0,
                parts: r.get(7)?,
            })
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut articles = conn
        .prepare(
            "select articles.message_id, articles.filename, articles.part, articles.total_parts, articles.bytes,
                articles.subject, releases.poster, releases.posted_date
             from articles join releases on articles.release_id = releases.id where articles.release_id = ?",
        )
        .unwrap();
    rows.into_iter()
        .map(|release| {
            let mut list: Vec<atlas::search::ArticleRow> = articles
                .query_map([release.id], |r| {
                    Ok(atlas::search::ArticleRow {
                        message_id: r.get(0)?,
                        filename: r.get(1)?,
                        part: r.get(2)?,
                        total_parts: r.get(3)?,
                        bytes: r.get(4)?,
                        subject: r.get(5)?,
                        poster: r.get(6)?,
                        posted_date: r.get(7)?,
                    })
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            list.sort_by(|a, b| (&a.filename, a.part, &a.message_id).cmp(&(&b.filename, b.part, &b.message_id)));
            (release.name.clone(), nzb::render_nzb(&release, &list), (release.size, release.complete, release.parts))
        })
        .collect()
}

#[test]
fn old_databases_convert_without_losing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let before = old_nzbs(&main);

    // opening it (the menu does) leaves it alone for the conversion
    db::create_db_at(&main).unwrap();
    assert!(convert::needed(&main));
    assert!(!store::exists(&main));

    let messages = std::cell::RefCell::new(Vec::new());
    let (releases, articles) = convert::run(&main, &|m| messages.borrow_mut().push(m.to_string())).unwrap();
    assert_eq!(releases, 40);
    assert_eq!(articles, (1..=40).map(|r| r % 7 + 1).sum::<i64>());
    assert!(messages.borrow().last().unwrap().contains("1 articles without a release left out"), "{messages:?}");

    assert!(!convert::needed(&main), "converted");
    assert!(store::exists(&main));
    assert!(dir.path().join("atlas.old.db").exists(), "the old database is kept aside");
    for shard in store::shard_paths(&main) {
        let mode: String = db::open_at(&shard).unwrap().query_row("pragma journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode, "wal", "shards are back on WAL after the fast fill");
    }
    db::create_db_at(&main).unwrap();

    // every release came over with the same NZB and stats, in its group's shard
    let conn = db::open_with_shards(&main).unwrap();
    for (name, nzb_before, stats_before) in &before {
        let (id, group): (i64, String) = conn
            .query_row("select id, group_name from releases where name = ?", [name], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(store::shard_of_id(id), store::shard_of(&group));
        let now = atlas::search::get_release_with(&conn, id).unwrap().unwrap();
        assert_eq!((now.size, now.complete, now.parts), *stats_before, "{name}");
        let nzb_after = nzb::render_nzb(&now, &store::articles(&conn, id).unwrap());
        assert_eq!(&nzb_after, nzb_before, "{name}");
    }
    // ids keep the old order: old id * 8 + shard
    let first: i64 = conn.query_row("select id from releases where name = 'Release.1'", [], |r| r.get(0)).unwrap();
    assert_eq!(first / store::SHARDS as i64, 3);

    // cursors carried over
    let cursor: (i64, i64, Option<i64>) = conn
        .query_row(
            "select live_cursor, backfill_cursor, last_article from groups where name = 'alt.binaries.a@news.x'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(cursor, (900, 500, Some(1000)));
    assert_eq!(store::totals(&conn).unwrap(), (40, articles));
    drop(conn);

    // new releases get ids after every old one
    let new = atlas::parser::Release {
        name: "Brand.New".into(),
        group: "alt.binaries.a".into(),
        articles: vec![atlas::parser::Article {
            message_id: "<new@x>".into(),
            filename: Some("n.bin".into()),
            part: Some(1),
            total_parts: Some(1),
            bytes: 5,
            ..Default::default()
        }],
        ..Default::default()
    };
    store::save(&main, &[new]).unwrap();
    let conn = db::open_with_shards(&main).unwrap();
    let newest: String =
        conn.query_row("select name from releases order by id desc limit 1", [], |r| r.get(0)).unwrap();
    assert_eq!(newest, "Brand.New");
}

#[test]
fn a_conversion_stopped_during_the_check_picks_up_there() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let before = old_nzbs(&main);
    db::create_db_at(&main).unwrap();

    // stopped as the check starts: the copy is done, nothing swapped
    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());
    assert!(convert::needed(&main), "the old database is still in place");

    let messages = std::cell::RefCell::new(Vec::new());
    convert::run(&main, &|m| messages.borrow_mut().push(m.to_string())).unwrap();
    assert!(messages.borrow().iter().any(|m| m.contains("copy finished earlier")), "{messages:?}");
    assert!(
        !messages.borrow().iter().any(|m| m.contains("sorting") || m.contains("% (")),
        "nothing copied again: {messages:?}"
    );
    assert!(!convert::needed(&main));

    let conn = db::open_with_shards(&main).unwrap();
    for (name, nzb_before, _) in &before {
        let id: i64 = conn.query_row("select id from releases where name = ?", [name], |r| r.get(0)).unwrap();
        let now = atlas::search::get_release_with(&conn, id).unwrap().unwrap();
        assert_eq!(&nzb::render_nzb(&now, &store::articles(&conn, id).unwrap()), nzb_before, "{name}");
    }
}

/// `--convert` has the database to itself for its whole run: refused while
/// the indexer (or a save) holds it, and a second conversion is refused
/// while one runs, each without touching anything.
#[test]
fn a_conversion_runs_alone() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();
    let new_main = dir.path().join("atlas.new.db");

    let writing = atlas::compact::try_hold_off_compaction(&main).unwrap().unwrap();
    let err = convert::run_alone(&main, &|_| {}).expect_err("converted while the indexer held the database");
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(convert::needed(&main) && !new_main.exists(), "nothing was done");
    drop(writing);

    let second = std::cell::RefCell::new(None);
    let converted = convert::run_alone(&main, &|_| {
        if second.borrow().is_none() {
            *second.borrow_mut() = Some(convert::run_alone(&main, &|_| {}).map(|_| ()));
        }
    })
    .unwrap();
    assert_eq!(converted.map(|(releases, _)| releases), Some(40));
    let err = second.into_inner().unwrap().expect_err("a second conversion ran alongside");
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(!convert::needed(&main), "the first one finished");
    assert_eq!(convert::run_alone(&main, &|_| {}).unwrap(), None, "nothing left to convert");
}

/// The indexer converting at start has the database to itself for the
/// conversion too: refused while another indexer or a `--convert` holds it,
/// and once converted it holds off compaction for indexing.
#[test]
fn an_indexer_converting_at_start_runs_alone() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let setup = || db::create_db_holding(&main).unwrap().unwrap();
    let new_main = dir.path().join("atlas.new.db");

    // another indexer that set up at the same time
    let other = atlas::compact::try_hold_off_compaction(&main).unwrap().unwrap();
    let err = atlas::bg_indexer::convert_at_start(&main, setup(), &|_| {}).err().expect("converted alongside another");
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(convert::needed(&main) && !new_main.exists(), "nothing was done");
    drop(other);

    // a `--convert` running
    let converting = std::cell::RefCell::new(None);
    let indexing = atlas::bg_indexer::convert_at_start(&main, setup(), &|_| {
        if converting.borrow().is_none() {
            *converting.borrow_mut() = Some(convert::run_alone(&main, &|_| {}).map(|_| ()));
        }
    })
    .expect("the indexer converted");
    let err = converting.into_inner().unwrap().expect_err("--convert ran alongside the indexer's conversion");
    assert!(format!("{err:#}").contains("in use"), "{err:#}");
    assert!(!convert::needed(&main));

    // indexing holds off a compaction, and nothing more is converted
    assert!(convert::run_alone(&main, &|_| {}).is_err(), "the indexing hold is held");
    drop(indexing);
    assert_eq!(convert::run_alone(&main, &|_| {}).unwrap(), None);
}

/// The swap folds the old database's WAL in before moving it aside as
/// atlas.old.db; a checkpoint a reader holds back (committed pages left only
/// in the WAL) refuses the swap instead of leaving a backup missing them.
#[test]
fn a_swap_whose_checkpoint_is_held_back_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();

    // a reader's snapshot keeps the write after it in the WAL
    let writer = Connection::open(&main).unwrap();
    writer.execute_batch("pragma wal_autocheckpoint = 0").unwrap();
    let reader = Connection::open(&main).unwrap();
    reader.execute_batch("begin; select count(*) from releases").unwrap();
    writer.execute_batch("create table late (x); insert into late values (7)").unwrap();

    let err = convert::run(&main, &|_| {}).unwrap_err();
    assert!(format!("{err:#}").contains("checkpoint"), "{err:#}");
    assert!(convert::needed(&main), "the old database is still in place");
    assert!(!dir.path().join("atlas.old.db").exists());

    // once the reader is gone the swap goes through, and the backup has the late write
    reader.execute_batch("commit").unwrap();
    drop((reader, writer));
    convert::run(&main, &|_| {}).unwrap();
    let late: i64 = Connection::open(dir.path().join("atlas.old.db"))
        .unwrap()
        .query_row("select x from late", [], |r| r.get(0))
        .unwrap();
    assert_eq!(late, 7);
}

/// A conversion that finished copying and was stopped before the swap only
/// picks up from that copy while the old database is the one it copied: one
/// written to (or put back from another backup) since is copied again, soo
/// nothing added to it is left out.
#[test]
fn a_stopped_conversion_copies_again_when_the_old_database_changed() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();

    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());

    // the old database gets a newer release after the copy
    {
        let conn = Connection::open(&main).unwrap();
        conn.execute(
            "insert into releases (id, name, group_name, poster, posted_date, size, complete, parts, file_total)
             values (500, 'Late.Release', 'alt.binaries.a', 'p', '2026-10-03 10:00:00', 9, 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "insert into articles (release_id, message_id, subject, filename, part, total_parts, bytes, file_total)
             values (500, '<late@x>', 's', 'late.bin', 1, 1, 9, 1)",
            [],
        )
        .unwrap();
    }

    let messages = std::cell::RefCell::new(Vec::new());
    let (releases, articles) = convert::run(&main, &|m| messages.borrow_mut().push(m.to_string())).unwrap();
    assert!(!messages.borrow().iter().any(|m| m.contains("copy finished earlier")), "{messages:?}");
    assert_eq!(releases, 41);
    assert_eq!(articles, (1..=40).map(|r| r % 7 + 1).sum::<i64>() + 1);
    let conn = db::open_with_shards(&main).unwrap();
    let id: i64 = conn.query_row("select id from releases where name = 'Late.Release'", [], |r| r.get(0)).unwrap();
    assert_eq!(store::articles(&conn, id).unwrap().len(), 1);
}

/// A change that keeps every count, id and cursor (here a rename) is still a
/// change to the old database: the stopped copy is thrown away, not trusted.
#[test]
fn a_stopped_conversion_copies_again_when_a_release_changed_under_the_same_counts() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();

    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());

    Connection::open(&main)
        .unwrap()
        .execute("update releases set name = name || '.v2' where name = 'Release.17'", [])
        .unwrap();

    let messages = std::cell::RefCell::new(Vec::new());
    convert::run(&main, &|m| messages.borrow_mut().push(m.to_string())).unwrap();
    assert!(!messages.borrow().iter().any(|m| m.contains("copy finished earlier")), "{messages:?}");
    let conn = db::open_with_shards(&main).unwrap();
    let renamed: i64 =
        conn.query_row("select count(*) from releases where name like '%.v2'", [], |r| r.get(0)).unwrap();
    assert_eq!(renamed, 1);
}

/// Scans of empty ranges move an old database's cursors without adding a
/// release or article: a stopped conversion copies again then too, soo the
/// swap doesnt put back the cursors from before and redo those scans.
#[test]
fn a_stopped_conversion_copies_again_when_only_the_cursors_moved() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();

    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());

    Connection::open(&main)
        .unwrap()
        .execute("update groups set backfill_cursor = 7 where name = 'alt.binaries.b'", [])
        .unwrap();

    let messages = std::cell::RefCell::new(Vec::new());
    convert::run(&main, &|m| messages.borrow_mut().push(m.to_string())).unwrap();
    assert!(!messages.borrow().iter().any(|m| m.contains("copy finished earlier")), "{messages:?}");
    let conn = db::open_at(&main).unwrap();
    let cursor: i64 =
        conn.query_row("select backfill_cursor from groups where name = 'alt.binaries.b'", [], |r| r.get(0)).unwrap();
    assert_eq!(cursor, 7);
}

/// Stopped between moving the old database aside and putting the new one in
/// place: the next start finishes the swap.
#[test]
fn a_swap_cut_after_the_old_database_moved_aside_is_finished_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let before = old_nzbs(&main);
    db::create_db_at(&main).unwrap();
    convert::run(&main, &|_| {}).unwrap();
    // as it was before the second rename
    std::fs::rename(&main, dir.path().join("atlas.new.db")).unwrap();

    drop(db::create_db_holding(&main).unwrap().expect("set up"));
    assert!(!dir.path().join("atlas.new.db").exists());
    assert!(dir.path().join("atlas.old.db").exists());
    assert!(!convert::needed(&main));
    let conn = db::open_with_shards(&main).unwrap();
    assert_eq!(store::totals(&conn).unwrap().0, before.len() as i64);
}

/// A search or the dashboard during the swap, with no main database for a
/// moment, doesnt make one: opening is not setting up.
#[test]
fn a_read_during_the_swap_makes_no_main_database() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    assert!(db::open_with_shards(&main).is_err());
    assert!(db::open_at(&main).is_err());
    assert!(!main.exists(), "nothing made in the way of the new one");
}

/// An empty atlas.db made in the swap's window (by a build that still made
/// one on a read) is in the way of the swap: taken out, and the swap is
/// finished (atlas.new.db ready) or the old database put back.
#[test]
fn an_empty_main_database_made_during_the_swap_is_taken_out() {
    for (made_by_open, ready) in [(false, true), (true, true), (false, false), (true, false)] {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("atlas.db");
        old_database(&main);
        let before = old_nzbs(&main);
        db::create_db_at(&main).unwrap();
        if ready {
            convert::run(&main, &|_| {}).unwrap();
            std::fs::rename(&main, dir.path().join("atlas.new.db")).unwrap();
        } else {
            std::fs::rename(&main, dir.path().join("atlas.old.db")).unwrap();
        }
        // the interloper: zero bytes, or a database with nothing in it
        if made_by_open {
            rusqlite::Connection::open(&main).unwrap().query_row("pragma journal_mode = wal", [], |_| Ok(())).unwrap();
        } else {
            std::fs::write(&main, b"").unwrap();
        }

        drop(db::create_db_holding(&main).unwrap().expect("set up"));
        assert!(!dir.path().join("atlas.new.db").exists());
        if ready {
            assert!(!convert::needed(&main));
            let conn = db::open_with_shards(&main).unwrap();
            assert_eq!(store::totals(&conn).unwrap().0, before.len() as i64);
        } else {
            assert!(!dir.path().join("atlas.old.db").exists());
            assert!(convert::needed(&main), "the old database is back");
        }
    }
}

/// A main database with something in it is never taken for an interloper.
#[test]
fn a_main_database_with_tables_is_left_alone_beside_a_cut_swap() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    std::fs::copy(&main, dir.path().join("atlas.old.db")).unwrap();
    assert_eq!(convert::recover_cut_swap(&main).unwrap(), None);
    assert!(dir.path().join("atlas.old.db").exists());
    assert!(convert::needed(&main), "still the same one");
}

/// `--convert` after a swap cut short: atlas.db is missing, soo it isnt
/// `needed`, yet there is work: the swap is finished, not "nothing to convert".
#[test]
fn a_convert_after_a_cut_swap_finishes_the_swap() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let before = old_nzbs(&main);
    db::create_db_at(&main).unwrap();
    convert::run(&main, &|_| {}).unwrap();
    std::fs::rename(&main, dir.path().join("atlas.new.db")).unwrap();

    assert!(!convert::needed(&main));
    assert!(convert::to_do(&main), "the cut swap is something to do");
    assert_eq!(convert::run_alone(&main, &|_| {}).unwrap(), None);
    assert!(!dir.path().join("atlas.new.db").exists());
    assert!(!convert::to_do(&main), "and done");
    let conn = db::open_with_shards(&main).unwrap();
    assert_eq!(store::totals(&conn).unwrap().0, before.len() as i64);
}

/// Moved aside with a new main database that never got to the swap: the
/// next start puts the old database back, and converting goes on from it.
#[test]
fn an_old_database_moved_aside_without_a_finished_new_one_is_put_back() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();
    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());
    std::fs::rename(&main, dir.path().join("atlas.old.db")).unwrap();

    drop(db::create_db_holding(&main).unwrap().expect("set up"));
    assert!(!dir.path().join("atlas.old.db").exists());
    assert!(convert::needed(&main), "the old database is back");
    assert_eq!(convert::run_alone(&main, &|_| {}).unwrap().map(|(r, _)| r), Some(40));
}

/// Long after a finished conversion, atlas.db lost while atlas.old.db and
/// the shards (with everything indexed since) are still there: putting the
/// old database back would convert again and the fresh copy would wipe the
/// shards. Refused, saying what to do, and nothing is touched.
#[test]
fn a_lost_main_database_after_a_conversion_does_not_bring_the_old_one_back() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    db::create_db_at(&main).unwrap();
    convert::run(&main, &|_| {}).unwrap();
    let releases = store::totals(&db::open_with_shards(&main).unwrap()).unwrap().0;
    assert!(releases > 0);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", main.display()));
    }

    let refused = convert::recover_cut_swap(&main).unwrap_err().to_string();
    assert!(refused.contains("atlas.old.db") && refused.contains("shards"), "{refused}");
    assert!(dir.path().join("atlas.old.db").exists());
    assert!(!main.exists());
    for path in store::shard_paths(&main) {
        assert!(path.exists());
    }
}

/// Release ids with a huge gap (AUTOINCREMENT after a reset, a hand-set id):
/// the conversion keeps per release what shard it went to, not a slot for
/// every id up to the highest, also when it picks up a stopped run.
#[test]
fn a_huge_sparse_release_id_converts_without_a_slot_per_id() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    old_database(&main);
    let far = 1i64 << 56;
    {
        let conn = Connection::open(&main).unwrap();
        conn.execute(
            "insert into releases (id, name, group_name, poster, posted_date, size, complete, parts, file_total)
             values (?, 'Far.Away', 'alt.binaries.a', 'p <p@x>', '2026-10-02 10:11:12', 100, 1, 1, 1)",
            [far],
        )
        .unwrap();
        conn.execute(
            "insert into articles (release_id, message_id, subject, filename, part, total_parts, bytes, file_total)
             values (?, '<far@x>', '\"far.rar\" yEnc (1/1)', 'far.rar', 1, 1, 100, 1)",
            [far],
        )
        .unwrap();
    }
    let before = old_nzbs(&main);
    db::create_db_at(&main).unwrap();

    // stopped as the check starts, then picked up
    let stopped = std::panic::catch_unwind(|| {
        convert::run(&main, &|m| assert!(!m.contains("checking NZBs"), "stop here")).unwrap();
    });
    assert!(stopped.is_err());
    convert::run(&main, &|_| {}).unwrap();

    let conn = db::open_with_shards(&main).unwrap();
    assert_eq!(store::totals(&conn).unwrap().0, before.len() as i64);
    for (name, nzb_before, _) in &before {
        let id: i64 = conn.query_row("select id from releases where name = ?", [name], |r| r.get(0)).unwrap();
        let now = atlas::search::get_release_with(&conn, id).unwrap().unwrap();
        assert_eq!(&nzb::render_nzb(&now, &store::articles(&conn, id).unwrap()), nzb_before, "{name}");
    }
}
