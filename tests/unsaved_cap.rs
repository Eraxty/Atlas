//! The cap on headers fetched but not saved yet, over every pass at once:
//! many passes on few connections with a budget of two slices all finish,
//! every article is saved exactly once, the budget is never overrun and
//! nothing of it is left held afterwards.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use atlas::indexer::{PassContext, PassSettings, RunStates, run_pass, shared_db};
use atlas::nntp::Pool;
use common::{Server, mock, par2_file_desc, post, spawn_server, yenc_body};

const POSTS: u64 = 400;
const REQUEST: u64 = 20;
const BATCH: u64 = 100;

/// `POSTS` articles in releases of one slice each, every release with a par2,
/// soo saving a slice needs BODY lookups on the connections the XOVERs use
fn busy_server() -> Arc<Server> {
    let posts = (1..=POSTS)
        .map(|n| {
            let (rel, part) = ((n - 1) / REQUEST, (n - 1) % REQUEST);
            if part == 0 {
                let par2 = par2_file_desc(&format!("Real.Name.{rel}.mkv"), 9);
                post(n, &format!(r#""rel{rel}.par2" yEnc (1/1)"#), 10, yenc_body("rel.par2", &par2))
            } else {
                post(n, &format!(r#""rel{rel}.part{part}.rar" yEnc (1/1)"#), 10, vec![])
            }
        })
        .collect();
    let mut s = Server::new(posts);
    let s_mut = Arc::get_mut(&mut s).unwrap();
    s_mut.any_group = true;
    s_mut.xover_delay = Duration::from_millis(5);
    s
}

/// `groups` groups indexed to the end at once, on two servers of two
/// connections each, with room for `cap` unsaved headers.
fn index_everything(cap: usize, groups: usize) {
    let servers = [busy_server(), busy_server()];
    let ports: Vec<u16> = servers.iter().map(|s| spawn_server(s.clone())).collect();
    let pool =
        Arc::new(Pool::new(&[mock(ports[0], "secret", 2, 1), mock(ports[1], "secret", 2, 2)]).with_max_unsaved(cap));

    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("atlas.db");
    atlas::db::create_db_at(&main).unwrap();
    let db = shared_db(atlas::db::open_at(&main).unwrap());

    let ctx = Arc::new(PassContext {
        pool: pool.clone(),
        states: RunStates::default(),
        stop: Arc::new(AtomicBool::new(false)),
        verbose: false,
    });
    let settings = Arc::new(PassSettings {
        mode: "backfill".into(),
        batch_size: BATCH as i64,
        request_size: REQUEST,
        split_min_backlog: i64::MAX,
        ..Default::default()
    });

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    let saved: Vec<i64> = rt.block_on(async {
        pool.connect().await.unwrap();
        let mut passes = tokio::task::JoinSet::new();
        for g in 0..groups {
            let (ctx, settings, db) = (ctx.clone(), settings.clone(), db.clone());
            passes.spawn(async move {
                let group = format!("alt.binaries.cap{g}");
                let mut saved = 0;
                for _ in 0..50 {
                    let p = run_pass(&ctx, &settings, &db, &group, g % 2, &mut |_| {}).await.unwrap();
                    saved += p.articles;
                    if ctx.states.is_idle(&group) {
                        return saved;
                    }
                }
                panic!("{group} never went idle");
            });
        }
        let all = async {
            let mut saved = Vec::new();
            while let Some(r) = passes.join_next().await {
                saved.push(r.unwrap());
            }
            saved
        };
        tokio::time::timeout(Duration::from_secs(60), all).await.expect("the passes are stuck")
    });

    assert_eq!(saved, vec![POSTS as i64; groups], "every article saved once, per group");
    // slices no bigger than the budget, in passes of BATCH
    let slice = REQUEST.min(cap as u64);
    let xovers: usize = servers.iter().map(|s| s.xovers.load(Ordering::SeqCst)).sum();
    assert_eq!(xovers, groups * (POSTS / BATCH * BATCH.div_ceil(slice)) as usize, "every slice fetched once");

    let peak = pool.unsaved_peak();
    assert!(peak <= cap, "{peak} headers unsaved at once, the cap is {cap}");
    assert!(peak >= slice as usize, "the cap was never reached ({peak}), the test proves nothing");
    assert_eq!(pool.unsaved_headers(), 0, "budget left held after every pass ended");

    // the names came from the bodies: saving did its lookups
    let conn = atlas::db::open_with_shards(&main).unwrap();
    let (releases, named): (i64, i64) = conn
        .query_row("select count(*), count(display_name) from releases", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((releases, named), ((groups as u64 * POSTS / REQUEST) as i64, releases));
}

/// Two slices' worth for 16 passes of up to two connections each.
#[test]
fn many_passes_share_a_budget_of_two_slices() {
    index_everything(2 * REQUEST as usize, 16);
}

/// A budget smaller than one slice: the slices are cut down to it. Only one
/// slice fits at a time, soo every slice of every pass goes one after another
/// (fetch, then save): four passes, still more than the four connections and
/// both servers, keep it to a few hundred slices.
#[test]
fn a_budget_smaller_than_a_slice_still_gets_through() {
    index_everything(7, 4);
}
