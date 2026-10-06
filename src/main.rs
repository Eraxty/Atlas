use std::process::ExitCode;

use atlas::{app, bg_indexer, compact, convert, db, paths, procs, store};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|a| a == flag);

    if has("--version") || has("-V") {
        println!("atlas {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    if has("--help") || has("-h") {
        println!(
            "atlas {}\n\nusage: atlas [--selftest | --bg-indexer | --convert]\n\n  (no args)     interactive menu\n  --selftest    check the login on every usenet server and exit\n  --bg-indexer  run the indexing loop headless (the menu starts this for you)\n  --convert     move a database from before the shards into them, then exit\n                (the indexer does this on its own when it starts)\n  --compact     rewrite the shards smaller (stop indexing first), then exit",
            env!("CARGO_PKG_VERSION")
        );
        return ExitCode::SUCCESS;
    }

    let code = if has("--convert") {
        run_convert()
    } else if has("--compact") {
        run_compact()
    } else if has("--selftest") {
        app::selftest()
    } else if has(procs::BG_FLAG) {
        bg_indexer::run()
    } else {
        app::main_menu()
    };

    ExitCode::from(code.clamp(0, 255) as u8)
}

/// `--convert`: the one time move into the shards, in the foreground.
fn run_convert() -> i32 {
    let main = paths::database();
    // a swap cut short is finished (or undone) by `run_alone`, not skipped
    if !convert::to_do(&main) {
        println!("{} is already converted (or doesnt exist)", main.display());
        return 0;
    }
    if procs::indexer_alive() {
        println!("stop indexing first");
        return 1;
    }
    // alone: not alongside the indexer, a compaction or another conversion
    match convert::run_alone(&main, &|msg| println!("{msg}")) {
        Ok(Some(_)) => 0,
        Ok(None) => {
            println!("{} is already converted (or doesnt exist)", main.display());
            0
        }
        Err(e) => {
            println!("couldnt convert, nothing was changed: {e:#}");
            1
        }
    }
}

/// `--compact`: rewrite the shards smaller, in the foreground.
fn run_compact() -> i32 {
    if procs::indexer_alive() {
        println!("stop indexing first");
        return 1;
    }
    // nothing sets it: only the indexer stops a compaction early
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    match compact::run(&paths::database(), &|msg| println!("{msg}"), &stop) {
        Ok(_) => {
            // noted like the indexer's own compaction does, soo the auto one waits its interval
            // never made here: an empty main database would pass for the real one
            if let Ok(conn) = db::open_shard(&paths::database()) {
                let _ = store::set_meta(&conn, "last_compact", chrono::Utc::now().timestamp());
            }
            0
        }
        Err(e) => {
            println!("couldnt compact: {e:#}");
            1
        }
    }
}
