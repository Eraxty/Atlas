# poc_sqlite_2: a compact article layout and a split database

A proof of concept for three ideas to make the Atlas database smaller and faster:

- **Compact layout:** store less per article.
- **Split database:** spread groups over 8 database files with one writer each.
- **Memory segments:** write into an in-memory database, then save it to disk as a new file in one sequential write.

Nothing here changes Atlas itself.

## The compact layout

Today each article is a row in `articles` that holds its message ID, its full subject and its filename. A unique index on `(release_id, message_id)` stores the message ID a second time, and `release_files` adds a row per file.

The compact layout, in `src/compact.rs`:

- **`files`:** one row per file of a release. It holds the filename, the one subject the NZB uses, and the part bitmap that `release_files` holds today.
- **`segments`:** one row per article, keyed by `(file, message ID)` in a `WITHOUT ROWID` table. The message ID is stored once, with no second index, next to the part number and size.
- **compact2** also splits each message ID into a local part and a domain such as `@ngPost` or `@nyuu`. The domain is stored once in a `domains` table, and hex local parts are stored as bytes at half the size. Every message ID comes back exactly.

The `releases` table, the search index and its triggers don't change.

## Storage results

`storage` copies the newest 50,000 releases of the real database, with their 1,358,657 articles, through today's save code and through the compact one. It then compares the database sizes after `VACUUM`:

| Layout | Size | Bytes per article | Compared with today |
|---|---|---|---|
| Current | 383.2 MB | 282.0 | 1.00× |
| compact1 | 113.8 MB | 83.7 | 0.30× |
| compact2 | 98.6 MB | 72.6 | **0.26×** |

It also builds every sampled release's NZB from today's layout and from each compact layout, using Atlas's own `render_nzb`. All 50,000 NZBs are byte-for-byte identical, and so is each release's size, part count and completeness.

At 0.26×, today's 424 GB database would take about 110 GB.

## Speed results

`speed` fills a database in each layout with the same 100 million synthetic articles, then times saving 3 million new headers into a fresh clone of each. The live indexer kept running during the test, so expect about ±30% noise.

| Layout | Database size | Round 1 (headers/s) | Round 2 (headers/s) |
|---|---|---|---|
| Current | 24.1 GB | 35,507 | 24,871 |
| compact1 | 10.1 GB | 38,272 | 48,480 |
| compact2 | 6.9 GB | 35,876 | 38,207 |
| compact2, split into 8 databases | 6.8 GB in total | **101,746** | **127,008** |
| Memory segments | 2 GB in memory | 483,137 | 183,184 |

- **The compact layout saves space, not time.** With one writer it's about as fast as today.
- **Splitting into 8 databases is 3–5× faster than today**, with one writer per database.
- **Memory segments are the fastest to write**: writing a segment out to disk took 0.7–1.5 seconds. The test doesn't measure what this costs afterward: searches and NZB lookups would read every segment, segments need merging in the background, and a release that spans two segments has to be combined when it's read.

## Conclusions

- **The compact layout (compact2)** cuts storage to about a quarter, with byte-for-byte identical NZBs. On its own it doesn't make writes faster.
- **Splitting the database into 8 files** makes writes 3–5× faster than today.
- **Together** they're the change worth building: about a quarter of the space and several times the write speed.
- **Memory segments** could come later, inside each split database, once the read side is designed.

## Run it

```bash
cd poc_sqlite_2
cargo run --release -- storage --db ../atlas.db --sample 50000
cargo run --release -- speed --work target/work --prefill 100000000
```

`storage` opens the real database read-only. `speed` keeps its filled template databases in the work folder so later runs reuse them. Delete the folder to start over. It needs about 50 GB free on an APFS volume.
