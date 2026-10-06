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
| `shot(png)` | 35.8 ms | 60.2 ms | not a regression, [see below](#were-navigate-and-png-regressions) |
| `navigate` | 8.6 ms | 89.7 ms | p95 fixed, [see below](#were-navigate-and-png-regressions) |

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

## cu against agent-browser and Playwright MCP

Same browser binary for all three (Chrome for Testing headless shell 153 --
agent-browser does not start on the snap Chromium), the same local site and the
same six agent tasks. Every tool is driven the way an agent drives it: `cu` and
agent-browser through their CLIs, one process per action; Playwright MCP over
stdio JSON-RPC, one `tools/call` per action. Refs always come from the tool's
own snapshot.

```sh
CU_BROWSER=.../chrome-headless-shell python3 bench/compare.py --iters 10
```

All tools are up at once and each round runs every task on every tool in a
rotating order, so load on the shared machine falls on all of them alike. Median
of 10 rounds, in ms (lower is better):

| task | cu | cu (batch) | agent-browser | Playwright MCP |
| --- | ---: | ---: | ---: | ---: |
| cold start + first open | **206** | 220 | 321 | 2 437 |
| open page | 14 | **12** | 20 | 53 |
| snapshot | **3.7** | 3.8 | 11 | 12 |
| click | **5.5** | 6.7 | 8.1 | 567 |
| form (open, read, 2 fields, select, submit, read) | 83 | **73** | 162 | 727 |
| login (open, read, user, password + Enter, read) | **81** | 81 | 157 | 670 |
| 10 clicks in a row | 55 | **25** | 155 | 5 491 |

Snapshot of the same 2.3 KB catalogue page (12 nav links, 10 items with a
button each, search, footer), counted with tiktoken `o200k_base`:

| tool | tokens | bytes |
| --- | ---: | ---: |
| **cu** `snapshot` | **350** | 931 |
| agent-browser `snapshot -i` (interactive only) | 480 | 1 148 |
| agent-browser `snapshot` | 953 | -- |
| Playwright MCP `browser_snapshot` | 1 314 | 3 684 |

Notes, so the comparison is read fairly.

- `cu` and Playwright MCP wait for a navigation an action starts. agent-browser
  returns from `click`/`press` before a submit navigates, so its form and login
  include the `wait --url` + `wait --load` an agent has to add (without them its
  next snapshot still shows the form).
- Playwright MCP mints new refs after every action, so its "10 clicks" reads a
  snapshot before each click; its ~550 ms per click is its own post-action
  settle, not the harness. `cu` refs survive actions (see below).
- `cu (batch)` is `cu act` with the task's steps in one call; its form/login
  results include the closing snapshot, so no separate read is needed.
- Per-process CLI cost is part of the cu and agent-browser columns: on this
  machine spawning `/bin/true` alone costs several ms under load.

## Actions by ref, and batches

`POST /v1/act` (`cu act`) takes a list of actions and runs them on one DevTools
connection, ending with a snapshot. `/v1/click` and `/v1/type` (`cu click`,
`cu type`) are the one-action forms.

| | per action |
| --- | ---: |
| click, no navigation, inside a batch | ~1 ms |
| click, no navigation, one HTTP call | 3-5 ms |
| click that navigates (until the new DOM is ready) | 15-35 ms |

- **Stable refs.** The snapshot leaves a ref -> element registry in the page
  (held by `WeakRef`). An element keeps its ref across snapshots and actions;
  numbering carries over navigations, so an old ref misses with "take a new
  snapshot" instead of hitting a different element.
- **Real input.** Clicks are mouse events at the element's centre after a hit
  test; a covered element is reported (`e4 is covered by div`), not clicked
  through. Text goes in with `Input.insertText`, keys with `dispatchKeyEvent`.
- **Navigation without polling.** Chromium announces a navigation before it
  answers the input event that caused it. A no-op evaluate is used as a
  barrier for submits; an earlier version awaited `setTimeout(0)` instead and
  paid 12-15 ms per action, because headless Chromium runs the next task only on
  its next frame. Ten clicks went from ~170 ms to ~25 ms with the barrier.

## Browser CPU and memory

Same workload (20 x navigate + snapshot + screenshot, then 3 s idle), browser
tree only, snap Chromium 145, two runs each:

| | before | after |
| --- | ---: | ---: |
| processes | 11 | 10 |
| PSS | ~337 MB | ~316 MB |
| CPU time | ~1.65 s | ~1.15 s (-30%) |
| browser left running after the daemon stops | **all of it** | none |

- Light flags: no background networking, component updates, sync, extensions,
  crash upload or audio, no GPU raster, same-site frames share a renderer
  (`BROWSER_ARGS` in `src/server.rs`). `--in-process-gpu` would save one more
  process but crashes chrome-headless-shell on navigation, so it is not used.
- PSS is noisy on a machine running other Chromiums (shared pages are split
  between everyone mapping them); CPU and process count are the steadier
  signal. On chrome-headless-shell the flags change nothing measurable (8
  processes, ~180 MB either way).
- **Session close.** Stopping `cu` (SIGTERM/SIGINT) used to orphan the whole
  browser: 8-10 processes and a few hundred MB for every daemon ever started.
  It now sends `Browser.close` (the profile is flushed) and falls back to
  SIGTERM.
- **One browser, many contexts.** `?context=NAME` on navigate, snapshot,
  screenshot and act runs in an isolated browser context (own cookies, storage
  and cache) of the same Chromium. `GET /v1/contexts` lists them,
  `DELETE /v1/contexts/NAME` closes one.

  | | extra memory | ready in |
  | --- | ---: | ---: |
  | a second browser | ~176 MB | 0.5-1 s |
  | a context in the running browser | ~19 MB | ~35 ms |

## Were navigate and png regressions?

The results table above showed `navigate` 8.6 -> 89.7 ms and `shot(png)`
35.8 -> 60.2 ms. Re-measured with both binaries running side by side
(interleaved, 30 iterations, twice):

- **`shot(png)` was not a regression.** Interleaved, the binaries are within
  noise of each other (26-32 ms vs 29 ms). The earlier column came from runs
  at different times on a loaded machine.
- **`navigate` was half real.** The old binary returned before the page was
  usable, so the median is not comparable. The p95 was a real regression: the
  settle wait polled `readyState` every 25 ms, so a page that became ready just
  after a poll paid most of a tick, and its `MutationObserver` probe always
  reported 0, so it never detected anything. The page now resolves a promise on
  `DOMContentLoaded` (or at once) and the daemon awaits it in one round trip:

  | navigate (interleaved) | median | p95 |
  | --- | ---: | ---: |
  | polling settle (before) | 45 ms | 59 ms |
  | awaited settle (after) | **26 ms** | **50 ms** |

## Reproducing

```sh
python3 bench/cu_bench.py --iters 20            # prints JSON to stdout
python3 bench/cu_bench.py --iters 20 --out a.json
python3 bench/compare.py --iters 10             # cu vs agent-browser vs Playwright MCP
cargo test                                      # correctness, not speed
```

`CU_BROWSER` picks the browser for both harnesses. `compare.py` needs
`agent-browser` on the PATH and fetches `@playwright/mcp@0.0.83` with `npx`;
`--tools cu,cu-batch` runs `cu` alone.

Nothing is published and nothing leaves the machine: the daemon is loopback-only
and the benchmark talks to it with the token from the data directory it created.

## 50 parallel tests, one Chrome, one tab each

The resource model Jaime asked for: **one Chrome always open**, every test
opens its own tab in that Chrome and closes it at the end (a lease closes it
if the test dies). How much does that cost when 50 tests run at once, and
where does it saturate on this server (12 cores, 64 GB, already carrying
other jobs)?

### Method

`bench/parallel_tabs.py` starts one `cu` daemon (one Chromium, headless,
`/snap/bin/chromium`) and serves a 300-card page with live JS from a local
threaded HTTP server (no network variance). One **test** is the real agent
flow over the loopback API: `tab open URL --lease 120` -> `navigate` (waits
for settle) -> `snapshot` (read refs) -> `act` (type + click by ref) ->
`text` (judge "ordered 7 items") -> `tab close`. Concurrency is swept with
distinct tabs per test; `/proc` is sampled twice a second for CPU and RSS.
Machine had background load ~2.5 of 12 cores throughout ("cargado").

```sh
python3 bench/parallel_tabs.py --levels 5,10,20,35,50 --waves 2
```

### Results (one run, 2026-10-06, machine load1 ~2.5-3)

| concurrency | tests | errors | tests/s | flow p50 | flow p95 | Chrome CPU (cores, max) | Chrome RSS (max) | cu RSS (max) | machine busy (max) |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 5   | 9*  | 0 | 0.15* | 0.15 s | 0.17 s | 0.05 | 0.8 GB  | 2.1 MB  | 32 % |
| 10  | 20  | 0 | 12.4  | 0.29 s | 0.30 s | 0.12 | 1.0 GB  | 2.8 MB  | 38 % |
| 20  | 40  | 0 | 18.9  | 0.52 s | 0.56 s | 0.18 | 1.3 GB  | 4.3 MB  | 55 % |
| 35  | 70  | 0 | 22.7  | 0.96 s | 1.02 s | 0.33 | 1.5 GB  | 8.1 MB  | 63 % |
| 50  | 100 | 0 | 24.5  | 1.40 s | 1.50 s | 0.35 | 1.7 GB  | 10.6 MB | 69 % |

**Headline -- 50 tests in parallel, 50 distinct tabs, one Chrome:** 50/50
passed in 2.1 s wall (per flow: p50 1.52 s, max 1.62 s), peak **1.84 GB
Chrome RSS** (11 processes: browser + GPU + renderers, Chromium consolidates
same-site tabs per renderer), **11 MB for the cu daemon**, Chrome CPU
**0.31 cores** peak, machine busy peaked at 71 %.

\* The 5-concurrency wave hit one cold-start outlier: a single `cu` call hung
until the 60 s harness timeout (recorded as one missing flow; every later
level ran 0 errors). Reported as measured.

### What saturates first

- **Not Chrome.** Even at 50 tabs Chromium uses < 0.4 of 12 cores and 1.8 GB
  of 64 GB. Chrome is the resource being shared, not the bottleneck.
- **Throughput flattens around 25-35 concurrent flows** (tests/s: 12 -> 19 ->
  23 -> 24) while **per-flow latency grows ~linearly with concurrency**
  (p50: 0.29 s -> 1.40 s from 10 to 50). The bottleneck is the serialized
  part of the path -- six loopback round trips plus one `cu` process spawn
  per call, on a machine already at ~25 % load -- queuing in the daemon and
  the page-settle wait, not renderer CPU.
- **RAM grows roughly 18 MB per open tab** for a JS page of this size
  (0.8 GB at a few tabs -> 1.8 GB at 50). Trivial against 64 GB; a fleet of
  heavy media pages would move this number, not the model.

### Recommended concurrency on this server

- **8 concurrent tests by default** (browser-side p95 stays ~0.3 s per call,
  ~1 GB total, invisible to the other jobs on the box).
- **12 as the interactive cap** for agent runs: past ~12-20 every browser
  call starts queueing and an agent's step-by-step loop gets sluggish.
- **Up to 50 in one batch is survivable** (measured: 100/100 green at 50
  concurrency): fine for unattended CI-style sweeps where wall time matters
  more than per-step latency; p95 ~1.5 s per flow then.
- Leases (default 10 min) make the pile-up self-cleaning: a crashed run's
  tabs are reaped, so concurrency limits do not have to be perfect.
