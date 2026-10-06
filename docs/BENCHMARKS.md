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

| action | before (median) | after (median) | change |
| --- | ---: | ---: | ---: |
| `start` | 431.34 ms | **6.08 ms** | **71x** |
| `snapshot` | n/a (did not exist) | **1.72 ms** | new |
| `navigate` | 8.59 ms | 20.66 ms | see note |
| `shot` (png) | 35.75 ms | 35.62 ms | ~same |
| `shot(jpeg)` | n/a | 38.21 ms | new option |
| `shot x8 parallel` | 92.84 ms | 82.00 ms | **1.13x** |

Note on `navigate`: the two columns are not the same work. Before, `navigate`
returned as soon as Chromium accepted the URL -- a page an agent could not yet
act on. After, it waits for the page to settle (see "Smart waits") and returns
`settled_ms` saying what it spent. The comparable figure is the *old* time
*two* actions later, once the page was finally usable.

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
