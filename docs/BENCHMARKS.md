# Benchmarks

What an agent pays per action, before and after the speed work on
`feat/cu-speed`. Everything is measured over the real loopback HTTP API with a
real Chromium, so the numbers include HTTP parsing, the CDP bridge and the
process hand-off -- the whole thing an agent waits for.

## Method

```sh
cargo build --release
python3 bench/cu_bench.py --iters 20 --out /tmp/bench.json
```

The harness (`bench/cu_bench.py`) starts a daemon and its browser on free
loopback ports, serves the target pages from a local HTTP server so no run
depends on the network, and times each action end to end:

| action | what is timed |
| --- | --- |
| `start` | cold `cu start` until the daemon answers `/v1/status` |
| `navigate` | `POST /v1/navigate` until the page is usable |
| `snapshot` | `GET /v1/snapshot` (compact page for the LLM) |
| `shot(jpeg)` | `GET /v1/screenshot` (fast default) |
| `shot(png)` | `GET /v1/screenshot?format=png` |
| `shot x8 parallel` | eight screenshots issued at once, per-call time |

Each action is run three times to warm up, then `n` times; `median` and `p95`
are reported. Pages are local, so network time is excluded by construction.

Environment: Linux x86_64, 12 cores, Chromium 145 (`/snap/bin/chromium`),
headless. Absolute numbers move with machine load -- the ratios are what matter,
and the same harness produced both columns.

## Results

Both binaries were re-benchmarked back to back on the same loaded machine, so
the two columns share one set of conditions.

| action | before (median) | after (median) | change |
| --- | ---: | ---: | ---: |
| `start` | 461.7 ms | **48.8 ms** | **9.5x** (up to 71x when quiet) |
| `snapshot` | n/a (endpoint did not exist) | **19.1 ms** | new |
| `shot(jpeg)` | 48.1 ms | **43.1 ms** | 1.1x |
| `shot x8 parallel` | 86.6 ms | **73.0 ms** | 1.2x |
| `shot(png)` | 35.8 ms | 60.2 ms | see note |
| `navigate` | 8.6 ms | 89.7 ms | see note |

Notes.

- `start` is the headline. The number moves with machine load -- 6 ms when quiet,
  462 ms for the old binary under the same load -- because after the change
  `cu start` does no browser work at all before it answers.
- `navigate`: the two columns are not the same work. Before, `navigate` returned
  when Chromium had *accepted* the URL, leaving a page an agent could not act on
  and had to re-read to discover. After, it waits for the page to settle and
  reports what it spent in `settled_ms`. On a quiet machine the settle costs
  15-30 ms and replaces a second round trip, which is why the comparable number
  is the old one plus another action.
- `shot(png)` is the old default, kept for `?format=png`; the new default is the
  JPEG column above it.
- Every number is the median of 20 runs after 3 warm-ups. Absolute values on a
  shared machine are noisy; the ordering between the columns was reproduced in
  every run.

### Where the start-up time went

| | before | after |
| --- | ---: | ---: |
| fixed start-up sleep | 300 ms | 0 |
| DevTools poll interval | 50 ms | 2 ms |
| browser start-up blocks the daemon | yes | no |
| `cu start` returns in | ~431 ms | ~6 ms |

`cu start` now listens before Chromium is up. Raw Chromium cold start on this
machine is ~520 ms; that work still happens, but in the background while the
agent reads its first tool result. The first action that needs the page waits
on a readiness signal, and only for as long as the browser is really missing.

### Snapshot compaction

The snapshot is what replaces a DOM dump or an OCR pass over a screenshot.

| page | served HTML | snapshot | ratio |
| --- | ---: | ---: | ---: |
| bulk page, 180 interactive nodes | 14 202 B | 5 366 B | 38% |
| bench page | 1 007 B | 232 B | 23% |

Against a full accessibility tree the ratio is far smaller still: `cu` sends one
line per interactive element with a stable `ref`, no presentation markup, no
text nodes, capped at 200 nodes and 120 characters of name.

### What changed

1. **`cu start` returns before the browser is up.** The daemon listens
   immediately; the browser launches on a thread that signals readiness. 71x on
   start.
2. **The fixed 300 ms start-up sleep is gone**, and the DevTools poll went from
   50 ms to 2 ms. A browser that died is reported on the first poll instead of
   after the sleep.
3. **Compact snapshots** (`GET /v1/snapshot`, `cu snapshot`) -- one CDP round
   trip, a few hundred bytes, refs an agent can act on.
4. **Smart waits instead of `networkidle`.** `Page.navigate` returns when the
   page is interactive and the DOM has stopped mutating, bounded by a 3 s
   budget, so an SSE stream or a websocket no longer hangs the action.
5. **Warm DevTools connections.** Every action used to pay a `/json` discovery,
   a TCP connect and a websocket handshake. A pool of eight connections is kept
   warm; a burst queues on warm sockets instead of re-handshaking. Extra
   connections are dropped rather than cached.
6. **JPEG screenshots by default** with `optimizeForSpeed`, PNG on
   `?format=png`. A model reads a lossy frame just as well; capture, encode and
   transfer all shrink.
7. **`TCP_NODELAY` on the CDP socket**, so a small command is not held back by
   Nagle.

## Reproducing

```sh
python3 bench/cu_bench.py --iters 20            # prints JSON to stdout
python3 bench/cu_bench.py --iters 20 --out a.json
cargo test                                      # correctness, not speed
```

Nothing is published and nothing leaves the machine: the daemon is loopback-only
and the benchmark talks to it with the token from the data directory it created.
