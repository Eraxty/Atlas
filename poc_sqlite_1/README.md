# poc_sqlite_1: faster SQLite writes for the indexer

A proof of concept that measures which changes make the Atlas database writer faster on a real database that's bigger than RAM. Nothing here changes Atlas itself.

## Method

The program saves the same synthetic slices of Usenet headers with Atlas's own save code (`atlas::db::save_release_batches`). Each variant writes into fresh APFS clones of the real database, which `cp -c` makes instantly without touching the original.

- **Database:** `atlas.db`, 424 GB, on the MacBook's internal NVMe drive, with 32 GB of RAM.
- **Workload:** 3,000 slices of 1,000 headers over 90 groups, about 20 articles per release, per variant.
- **Rounds:** two, each running every variant once on fresh clones.
- **Caveat:** the live indexer kept running during the test, so each result has about ±20% noise.

| Variant | What changes |
|---|---|
| `baseline` | What the indexer does now: 8 slices per transaction, and the writer finishes a checkpoint after every transaction. |
| `sorted` | Saves each transaction's releases in `(name, group)` order. |
| `wal256` | The writer finishes a checkpoint only once the WAL reaches 256 MB. |
| `b32` | 32 slices per transaction. |
| `shardsN` | N writers in parallel, each on its own database, with groups split between them. Each shard is a clone of the whole database, which is the worst case: real shards would be 1/N the size. |

## Results

Headers saved per second:

| Variant | Round 1 | Round 2 |
|---|---|---|
| `baseline` | 38,380 | 45,250 |
| `sorted` | 32,555 | 48,968 |
| `wal256` | 35,963 | 34,350 |
| `sorted+wal256` | 39,009 | 52,052 |
| `sorted+wal256+b32` | 33,369 | 46,266 |
| `sorted+wal256+shards4` | 66,591 | 51,291 |
| `sorted+wal256+shards8` | **73,122** | **66,631** |

## Conclusions

- **Sorting, a larger WAL, and larger transactions don't help.** With one writer, every variant lands within the noise of the baseline.
- **Parallel writers help, but less than linearly.** Eight shards save 1.5–1.9× as fast as one writer, even in this worst case where every shard is full size.
- **Disk space limits Atlas more than write speed does.** Each article costs about 195 bytes on disk, including indexes. At about 58,000 articles per second the database grows by about 24 GB an hour, which fills the remaining 1.5 TB in about 2.5 days. A full backfill of the 635 billion article numbers your servers report would need on the order of 100 PB.

## Run it

```bash
cd poc_sqlite_1
cargo run --release -- --db ../atlas.db --work target/poc-work
```

Options: `--variants` takes a comma-separated list such as `baseline,sorted+wal256+shards8`, `--rounds N`, `--slices N`, `--slice-size N`, `--per-release N`, and `--groups N`. The work folder must be on the same APFS volume as the database. The program deletes its clones when each variant finishes.
