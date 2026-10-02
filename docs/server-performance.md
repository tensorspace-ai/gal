# Server performance review

Measured locally on 2026-10-02 using release binaries. These are short,
controlled workloads on one machine, not production capacity estimates.

## Full-text indexing on the edit path

Before this change, replacing or deleting a search entry used
`DELETE FROM blip_search WHERE blip_id = ?`. FTS5's `blip_id` is unindexed:
SQLite scanned the whole search table while holding the write transaction.
Schema v7 preserves existing search row IDs in `blip_search_keys` and uses
indexed integer lookups. The migration does not rebuild or retokenize content.

The retained probe starts a server with a throwaway database, registers four
accounts, opens independent sockets and waves, and populates 100,000 search
entries outside those waves. Each socket submits another edit only after its
previous acknowledgement arrives. Setup and final storage checks are outside
the timed section. Every final document and revision is checked in SQLite.

```sh
python3 -m pip install aiohttp
cargo build --release -p gal-server
python3 tools/benchmark-server.py --sockets 8 --edits 40
python3 tools/benchmark-server.py                 # 16 sockets, 80 edits each
```

Two matched eight-socket runs, with no build or browser suite running:

| Implementation | Acknowledged edits | Time (seconds) | Edits/second | Median acknowledgement (ms) | p95 (ms) |
| --- | ---: | ---: | ---: | ---: | ---: |
| Before, run 1 (`9573f56`) | 320 | 3.179 | 100.7 | 8.968 | 20.356 |
| Before, run 2 (`9573f56`) | 320 | 3.131 | 102.2 | 8.992 | 22.276 |
| Integer lookup, run 1 | 320 | 0.136 | 2355.2 | 0.228 | 3.356 |
| Integer lookup, run 2 | 320 | 0.113 | 2842.0 | 0.235 | 3.429 |

The new path also completed 1,280 edits across 16 sockets in 0.318 seconds
(4,026.8 edits/second, median 0.186 ms, p95 4.032 ms), with all 16 persisted
documents verified. An earlier 16-socket run of the old path, while other
validation was running, encountered a persistence refusal; it is not used to
calculate a throughput ratio.

The workload has small documents and warm caches. It isolates the cost of
updating one entry in a large search index. It does not measure large document
composition, slow storage, long-running FTS segment maintenance, or hot-wave
fanout. The gate separately exercises multi-client convergence, private-wavelet
isolation, playback, attachments, and the browser editor.
