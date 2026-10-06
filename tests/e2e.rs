//! Index a fake usenet server end to end: NNTP -> parser -> sqlite -> search/nzb/api.
//!
//! Kept to a single test: it points ATLAS_HOME at a temp dir, and changing the
//! environment is only sound while no other thread is running.

mod common;

use atlas::{api, db, indexer::Indexer, nntp::BlockingPool, nzb, search};
use common::*;

#[test]
fn index_search_nzb_api() {
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this is the only test in this binary and nothing else has
    // spawned a thread yet, soo no one can read the environment concurrently
    unsafe {
        std::env::set_var("ATLAS_HOME", home.path());
        std::env::set_var("ATLAS_NO_KEYRING", "1");
    }

    // plain (not yEnc) nfo with a line that needs dot-stuffing
    let nfo: Vec<Vec<u8>> = vec![
        b"some ascii art".to_vec(),
        b".leading dot line".to_vec(),
        b"Release Name: Cool Movie 2024 1080p".to_vec(),
    ];
    let par2 = [par2_file_desc("small.nfo", 100), par2_file_desc("Hidden.Show.S01E01.mkv", 9_000_000)].concat();

    let mut posts = vec![
        post(1, r#"[1/3] - "Cool.Movie.2024.1080p.part1.rar" yEnc (1/2)"#, 1000, vec![b"x".to_vec()]),
        post(2, r#"[1/3] - "Cool.Movie.2024.1080p.part1.rar" yEnc (2/2)"#, 1000, vec![b"x".to_vec()]),
        post(3, r#"[2/3] - "Cool.Movie.2024.1080p.part2.rar" yEnc (1/1)"#, 500, vec![b"x".to_vec()]),
        post(4, r#"[3/3] - "Cool.Movie.2024.1080p.nfo" yEnc (1/1)"#, 50, nfo),
        post(5, r#"[1/2] - "a1B2c3D4e5F6g7H8i9J0.part1.rar" yEnc (1/1)"#, 4000, vec![b"x".to_vec()]),
        post(
            6,
            r#"[2/2] - "a1B2c3D4e5F6g7H8i9J0.par2" yEnc (1/1)"#,
            200,
            yenc_body("a1B2c3D4e5F6g7H8i9J0.par2", &par2),
        ),
        post(7, "total garbage subject", 1, vec![]),
    ];
    // a half posted release
    posts.push(post(8, r#""Broken.Upload.2024.mkv" yEnc (1/3)"#, 700, vec![]));

    let state = Server::new(posts);
    let port = spawn_server(state.clone());

    db::create_db().unwrap();

    // wildcards and empty groups
    assert!(!mock(port, "secret", 1, 1).use_ssl());
    let probe = BlockingPool::new(&[mock(port, "secret", 2, 1)]);
    probe.connect().unwrap();
    let groups = probe.list_groups(Some("ALT.binaries*")).unwrap();
    assert_eq!(groups.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(), vec![GROUP]);
    probe.disconnect();

    let bad = BlockingPool::new(&[mock(port, "wrong", 2, 1)]);
    assert_eq!(bad.connect().unwrap_err().code(), Some(481));

    // backfill everything
    let client = BlockingPool::new(&[mock(port, "secret", 4, 1)]);
    client.connect().unwrap();
    let mut indexer = Indexer::new(client, "backfill", db::open().unwrap());
    index_until_idle(&mut indexer);

    let hits = search::search_all_releases("cool movie", 0, 10).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    let cool = &hits[0];
    assert_eq!(cool.name, "Cool Movie 2024 1080p", "nfo name should win");
    assert!(cool.complete);
    assert_eq!(cool.parts, Some(4));
    assert_eq!(cool.size, Some(2550));
    assert_eq!(cool.posted_date.as_deref(), Some("2026-10-02 10:11:12"));
    assert_eq!(search::count_all_releases("cool movie").unwrap(), 1);
    assert_eq!(search::count_releases("cool", GROUP).unwrap(), 1);
    assert_eq!(search::count_releases("cool", "alt.binaries.other").unwrap(), 0);

    // obfuscated release got its name from the par2
    let hidden = search::search_all_releases("Hidden.Show", 0, 10).unwrap();
    assert_eq!(hidden.len(), 1, "{hidden:?}");
    assert_eq!(hidden[0].name, "Hidden.Show.S01E01.mkv");
    assert_eq!(search::count_obfuscated().unwrap(), 1);

    let broken = search::search_all_releases("broken upload", 0, 10).unwrap();
    assert_eq!(broken.len(), 1);
    assert!(!broken[0].complete);

    // fts syntax errors fall back to LIKE instead of blowing up
    assert!(search::search_all_releases("\"(", 0, 10).is_ok());

    // nzb
    let xml = nzb::build_nzb(cool.id).unwrap().unwrap();
    assert_eq!(xml.matches("<file ").count(), 3);
    assert_eq!(xml.matches("<segment ").count(), 4);
    assert!(xml.contains(">msg2@mock</segment>"));

    // newznab api
    let r = api::handle("/api", "t=search&q=cool&apikey=k", "http://atlas:9090", "k");
    assert_eq!(r.status, 200);
    assert!(r.body.contains("<title>Cool Movie 2024 1080p</title>"));
    assert!(r.body.contains("<pubDate>Fri, 02 Oct 2026 10:11:12 +0000</pubDate>"));
    assert!(r.body.contains(&format!("http://atlas:9090/api?t=get&amp;id={}&amp;apikey=k", cool.id)));
    assert!(r.body.contains(r#"<newznab:response offset="0" total="1"/>"#));

    let recent = api::handle("/api", "t=search&apikey=k&limit=500", "http://atlas:9090", "k");
    assert!(recent.body.contains("Hidden.Show.S01E01.mkv"));
    assert!(!recent.body.contains("a1B2c3D4e5F6g7H8i9J0<"), "raw obfuscated names stay hidden");

    let got = api::handle("/api", &format!("t=get&id={}&apikey=k", cool.id), "http://x", "k");
    assert_eq!(got.status, 200);
    assert_eq!(got.content_type, Some("application/x-nzb"));
    assert_eq!(got.body, xml);
    assert!(got.headers[0].1.contains(&format!("Cool Movie 2024 1080p.{}.nzb", cool.id)));
    assert_eq!(api::handle("/api", "t=get&id=99999&apikey=k", "http://x", "k").status, 404);

    // re-indexing the same range doesnt duplicate anything
    let before = search::all_releases(0, 100).unwrap().len();

    // live mode picks up new posts only
    state.posts.lock().unwrap().push(post(9, r#""New.Thing.2026.mkv" yEnc (1/1)"#, 10, vec![]));
    indexer.set_mode("live");
    index_until_idle(&mut indexer);
    assert_eq!(search::search_all_releases("new thing", 0, 10).unwrap().len(), 1);
    assert_eq!(search::all_releases(0, 100).unwrap().len(), before + 1);

    // a server renumber resets the cursors without crashing
    state.posts.lock().unwrap().retain(|p| p.number <= 3);
    index_until_idle(&mut indexer);

    // purge broken
    let conn = db::open().unwrap();
    let broken_before: i64 =
        conn.query_row("select count(*) from releases where complete = 0", [], |r| r.get(0)).unwrap();
    assert!(broken_before >= 1);
    db::purge_broken().unwrap();
    let broken_after: i64 =
        conn.query_row("select count(*) from releases where complete = 0", [], |r| r.get(0)).unwrap();
    assert_eq!(broken_after, 0);
    assert_eq!(search::search_all_releases("cool movie", 0, 10).unwrap().len(), 1);
}
