# Faster backfill and smaller storage: design

Date: 2026-10-05. Status: approved in chat, pending spec review.

## Context

Atlas indexes about 220k headers/s across 7 usenet servers. The database is 8 SQLite shards (`atlas.s0.db` .. `atlas.s7.db`, split by group) plus `atlas.db` (group cursors, id counter); see `src/store.rs`. Each article is a `segments` row: `(file_id, local BLOB, domain, part, bytes)`, keyed `(file_id, local, domain)` in a WITHOUT ROWID table. `local` is the message-id's local part (hex packed into bytes, anything else stored as text) and `domain` points at a shared `domains` row (0 = the whole message-id stored in `local`). One shard holds about 9.8M files and 221M segments, the average `local` is 32 bytes, and the shards total 138.5 GB.

Two limits remain:

- **Speed:** each group is pinned to one server (cursors are per server because article numbers differ by provider). Big groups and groups only a small server carries pile onto that server (super.newsgroupdirect.com, 15 connections, is always full) while other servers have idle connections.
- **Storage:** about 80 bytes per article, which full retention multiplies into many terabytes.

This design adds four things, built in this order:

1. Split big groups' backfill across servers by date
2. Pack message-id local parts by shape
3. Seal finished files into one compressed blob each
4. `auto_run_compact` in config.json

## 1. Split backfill by date

### Behavior

A group is **split** when at least 2 indexing servers carry it and its home server still has more than `SPLIT_MIN_BACKLOG` (10,000,000) article numbers of backfill left. Its remaining history is cut into **day chunks** (UTC days). Any worker on a server that carries the group may claim the next pending chunk, index that day's articles on its own server, and mark the chunk done. Live indexing (new posts) stays on the group's home server, unchanged.

Workers prefer their own groups. A worker with nothing of its own to do (all its groups busy, resting or caught up) claims a chunk instead. The newest pending chunk goes first, so history fills in from the present backwards like normal backfill.

### Chunk table (atlas.db)

```sql
create table if not exists backfill_chunks (
    grp TEXT NOT NULL,
    day INTEGER NOT NULL,          -- unix day (days since 1970-01-01 UTC)
    state INTEGER NOT NULL,        -- 0 pending, 1 claimed, 2 done
    server TEXT,                   -- host that claimed / did it
    claimed_at INTEGER,            -- unix seconds
    primary key (grp, day)
) without rowid;
```

- **Creating chunks:** when a group first qualifies, Atlas creates its chunks from the day of its home backfill frontier (the post date at the home cursor, from `groups.low_posted`, or now if unknown) back to the oldest day any carrying server retains. That oldest day is the post date of the article at the server's GROUP low-water mark, found with a single-article XOVER.
- **Stale claims:** a claim older than `CHUNK_CLAIM_TIMEOUT` (30 min) is pending again (its worker died or the indexer was stopped).
- **Home cursor:** once a group is split, the home server's backfill cursor no longer drives backfill. Its passes take chunks like any other server's.

### Day to article numbers

`Pool::article_at(server, group, unix_time) -> u64` finds the first article posted at or after `unix_time`. It binary-searches the server's article range with `XOVER n-n` (one article). Missing article numbers (423) are skipped forward to the next article that exists, up to 100 at a time. Results are cached in memory per `(server, group, day)`.

A chunk for day D covers articles posted from `D - 1 hour` to `D + 1 day + 1 hour`. Post dates aren't perfectly ordered by article number, so the overlap avoids gaps at the edges; duplicates are rejected by the segments key. The chunk's number range is fetched and saved with the existing `stream_headers` and `save_slice` path, in `request_size` slices.

### Failure handling

- **A server dies mid-chunk:** the chunk stays claimed until it times out, then is retried. Articles already saved stay and are deduplicated on the retry.
- **The binary search fails** (server error): the chunk is released and the error counted like a pass error.
- **A server doesn't carry the group** (411): that server is skipped for the group's chunks.

### Dashboard

On the Backfill page, a split group's progress counts done chunks. The "days per day" figure uses chunks done per hour across all servers.

## 2. Pack message-id locals by shape

`store::pack_local` picks the smallest alphabet the local part fits and stores `[tag][len][bytes]`, where `bytes` is the local part read as a base-N big number:

| Tag | Alphabet | N |
|---|---|---|
| 1 | `0-9a-f` (existing hex, no length byte, kept for compatibility) | 16 |
| 2 | `0-9A-F` (existing) | 16 |
| 3 | `0-9` | 10 |
| 4 | `0-9a-z` | 36 |
| 5 | `0-9A-Z` | 36 |
| 6 | `0-9A-Za-z` (Nyuu: letters plus timestamp) | 62 |
| 7 | `0-9A-Za-z-_` | 64 |
| 0 | anything else, stored as text (existing) | n/a |

- **Length byte:** it restores leading zero-digits, and the local part must be 1–255 characters. Longer ones fall back to tag 0.
- **Hex:** tags 1 and 2 stay as they are, so existing rows don't change meaning.
- **Odd-length hex:** goes to tag 4 or 5.
- **Encoding is deterministic:** the same message-id always encodes the same way.

**Duplicates across encodings:** a row saved before this change may hold the same message-id as text (tag 0). When `add_segment` packs a local into tag 3–7, it also checks for the tag-0 form under the same `(file_id, domain)`, the way it already checks the whole-id form.

**Existing data:** `--compact` re-encodes every local part into the new form. Expected saving: about 20–25% of the `local` column, around 8 bytes per article on base62 and base64 IDs.

## 3. Seal finished files into blobs

### Schema (each shard)

```sql
alter table files add column blob BLOB;          -- null until sealed
alter table files add column touched_at INTEGER; -- unix seconds of the last article added
```

`touched_at` is set by `put_file` whenever articles are added. Existing rows start with null, meaning untouched since the upgrade.

### Blob format v1

The blob is zstd (level 6) over:

```
u8 version = 1
varint count
for each segment, sorted by (part with a missing part first, message-id):
    varint part_plus_one            -- 0 = no part number
    zigzag varint bytes_delta       -- bytes minus the previous segment's bytes (first: minus 0)
    varint domain
    varint local_len, local bytes   -- the packed local from section 2
```

The order is exactly the order NZBs list segments in, so a blob decodes straight into `ArticleRow`s. Only part numbers, sizes and message-ids are needed; the subject, poster and date come from the `files` and `releases` rows, as today.

### Sealing

A file is eligible when it isn't sealed yet and either:

- it's complete: its `seen` bitmap is exactly parts 1..`expected`, or
- `touched_at` is older than 3 days. A null `touched_at` counts as old, so existing data seals in the first compaction.

To seal: read the file's segment rows, encode the blob, write `files.blob`, and delete the file's segment rows, all in one transaction.

Sealing happens in two places:

- **Online:** each shard writer, between save transactions, seals up to `SEAL_PER_TICK` (2,000) eligible files. It picks them from files touched in its recent transactions (complete ones) and from a cursor walking the `files` table by id (aged ones), so it never scans everything at once. Sealing time counts toward writer busy time and shows on the Bottleneck page.
- **Offline:** `--compact` seals every eligible file in its copy pass.

### Reading

`store::articles` decodes the blob, if there is one, and merges it with any segment rows for the file, which are articles that arrived after sealing. It then sorts by (filename, part, message-id) as today.

### Late articles

`add_segment` for a sealed file decodes the blob once per writer transaction (cached per file id) and skips any article already in it. New articles become ordinary segment rows. A sealed file with late rows is eligible again 3 days after its last touch and is resealed into a single blob.

### Size

Expected about 15–27 bytes per article inside blobs, versus about 55–70 as rows. The 138 GB total should drop to roughly 60 GB once compacted.

## 4. auto_run_compact

- **Setting:** `config.json` gains `"auto_run_compact": false` (the default, `Config::auto_run_compact()`).
- **When it runs:** when true, the background indexer checks once a minute whether 24 hours have passed since the last compaction. The time is stored in `atlas.db` as `meta('last_compact', unix seconds)`; without a stored time, it counts from when the indexer started. When the time is due:
  1. Set wind-down: workers finish their passes and start no new ones, as on a config change.
  2. Once `run_servers` returns (writers and checkpointer closed), run `compact::run` in the same process. The status label shows `compacting the database`, plus `compact::run`'s progress lines.
  3. Store `last_compact` and resume indexing.
- **Errors:** a compaction error is logged and counted. The originals are untouched by design, indexing resumes, and the next try is 24 hours later.
- **Manual runs:** `atlas --compact` still refuses to run while the indexer is running.

## Testing

- **Codec:** round trips for every tag over real-shaped message-ids (Nyuu, ngPost hex, JBinUp, camelsystem, random alphanumeric, odd lengths, longer than 255, empty), plus a check that encoding is deterministic.
- **Blob:** encode/decode round trips; NZBs byte for byte equal before and after sealing on generated releases, including ties, missing parts and missing filenames.
- **Late articles:** a sealed file plus a late article gives no duplicate and a correct NZB. A re-sent article that is already in the blob isn't saved.
- **Legacy duplicates:** a tag-0 row plus the same article re-sent packed gives no duplicate.
- **article_at:** against a mock server whose articles have known post dates, including gaps (423) and out-of-order dates.
- **Chunk queue:** two mock servers carry the same group with different article numbering, and the group is split. Both servers do chunks. Every article is indexed exactly once, and stale claims are taken over.
- **Compact:** re-encodes and seals. NZBs, row counts and stats are checked against the original (extends the existing `compact` test).
- **auto_run_compact:** with a 24-hour interval injected as a short duration in the test, the indexer winds down, compacts, records `last_compact`, and resumes.
- Everything passes `just format`, `just lint`, `just test`.

## Out of scope

- Pooling connections across servers that share article numbering (news.newsgroupdirect and viper do, but viper has 3 connections, so it's a small gain).
- Pipelining, XZVER, and request-size changes (measured: little gain under server-side latency).
- Dropping incomplete old releases (conflicts with keeping all history).
