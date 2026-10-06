//! `atlas --compact` notes the time like the indexer's own compaction does,
//! soo auto compaction doesnt run again right after it.

use std::process::Command;

#[test]
fn a_successful_compact_command_records_when_it_ran() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let last = || {
        let conn = atlas::db::open_at(&main).unwrap();
        atlas::store::get_meta(&conn, "last_compact").unwrap()
    };
    assert_eq!(last(), None);

    let before = chrono::Utc::now().timestamp();
    let out = Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--compact")
        .env("ATLAS_HOME", home.path())
        .env("ATLAS_NO_KEYRING", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let at = last().expect("last_compact is set");
    assert!(at >= before && at <= chrono::Utc::now().timestamp(), "{at}");
}

#[test]
fn a_compact_command_refuses_shards_whose_main_database_is_missing() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", main.display()));
    }

    let out = Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--compact")
        .env("ATLAS_HOME", home.path())
        .env("ATLAS_NO_KEYRING", "1")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stdout).contains("missing"), "{}", String::from_utf8_lossy(&out.stdout));
    // not made empty: the next start would hand out the shards' ids again
    assert!(!main.exists());
    assert!(atlas::db::create_db_at(&main).is_err(), "the next start still refuses");
}

#[test]
fn a_compact_command_finishes_a_conversion_swap_cut_short_first() {
    let home = tempfile::tempdir().unwrap();
    let main = home.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    // cut short after moving the main database aside: it's put back
    std::fs::rename(&main, home.path().join("atlas.old.db")).unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", main.display()));
    }

    let out = Command::new(env!("CARGO_BIN_EXE_atlas"))
        .arg("--compact")
        .env("ATLAS_HOME", home.path())
        .env("ATLAS_NO_KEYRING", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(main.exists());
    let conn = atlas::db::open_at(&main).unwrap();
    assert!(atlas::store::get_meta(&conn, "last_compact").unwrap().is_some());
}
