# cu — computer use for AI agents

`cu` runs **one persistent Chromium** behind a loopback-only HTTP server.
Agents get a small CLI, a Rust SDK and a skill for browser work; users enter
credentials on a local HTML form so passwords never pass through chat or
model-visible arguments.

An agent pays for every browser action twice: in milliseconds and in tokens.
`cu` cuts both — a snapshot is **350 tokens and 3.7 ms**, ten clicks in a batch
take **25 ms**, and **50 tests can run in parallel on one shared Chrome**, one
leased tab each.

Landing page: <https://jaivial.github.io/cu/> · API: [docs/README.md](docs/README.md) ·
Benchmarks: [docs/BENCHMARKS.md](docs/BENCHMARKS.md) · Skill: [SKILL.md](SKILL.md)

## Quick start

```sh
cargo run -- start          # one daemon per machine; answers before the browser is up
cargo run -- navigate https://example.com
cargo run -- snapshot       # the page in a few hundred bytes, ref-addressable
cargo run -- click e3
cargo run -- batch 'type e2 "Ada"' 'click e5'   # many actions, one call
```

## What `cu` gives an agent

- **One Chrome, always open.** A singleton daemon (found from any directory)
  keeps one Chromium warm; no browser is launched per action or per test.
- **Tabs and contexts with leases.** `cu tab open URL --lease 300` gives a test
  its own tab in that Chrome; `cu context open NAME` gives it an isolated
  cookie jar. A lease reaps the tab when the test dies, is cut off or the agent
  disappears — cleanup does not depend on the happy path. `cu lease` shows what
  is held; `cu tab renew` extends it.
- **Snapshots, not screenshots.** One line per interactive element with a
  stable `ref` that survives actions: `cu click e3`, `cu type e2 "text"`.
  Iframes are walked (refs reach into embedded documents) and so are open
  shadow roots, so web components do not snapshot as blank pages.
- **Batches by ref.** `cu act '[…]'` runs a whole form on one connection and
  returns the resulting page in the same call; `cu batch` sends many CLI
  commands at once and prints one compact JSON line per command.
- **Text when words are the answer.** `GET /v1/text` (`cu text`) returns the
  page's visible words — prose, API responses, messages — including inside
  frames and open shadow roots, capped at 16 000 characters.
- **Downloads land where the agent can see them.** Files go to
  `<data>/downloads` (per-context subfolders included), `cu downloads` lists
  them and the click that started one names it.
- **Skill and native tool.** [SKILL.md](SKILL.md) teaches an agent the whole
  flow (agentic tests, user stories, failure reports), and mini-tui exposes `cu`
  as a **native tool** next to `bash` — no MCP server in between.
- **Passwords never reach the model.** `cu login` prints a local form; the
  daemon types what the human enters. Credentials are never logged, returned
  or passed as arguments.

## How `cu` compares

All numbers are copied from [docs/BENCHMARKS.md](docs/BENCHMARKS.md) — same
browser binary (Chrome for Testing headless shell 153), same local site, same
agent tasks, median of 10 rounds, every tool driven the way an agent drives it.

### Speed, per task (ms, lower is better)

| task | cu | cu (batch) | agent-browser | Playwright MCP |
| --- | ---: | ---: | ---: | ---: |
| cold start + first open | **206** | 220 | 321 | 2 437 |
| open page | 14 | **12** | 20 | 53 |
| snapshot | **3.7** | 3.8 | 11 | 12 |
| click | **5.5** | 6.7 | 8.1 | 567 |
| form (open, read, 2 fields, select, submit, read) | 83 | **73** | 162 | 727 |
| login (open, read, user, password + Enter, read) | **81** | **81** | 157 | 670 |
| 10 clicks in a row | 55 | **25** | 155 | 5 491 |

The best of each row is bold. Reading it fairly: `cu` and Playwright MCP wait
for a navigation an action starts (agent-browser's form/login include the
`wait` calls an agent would have to add); Playwright MCP mints new refs after
every action, so its "10 clicks" reads a snapshot before each one; and
per-process CLI cost is included in the `cu` and agent-browser columns.

### Snapshot tokens (2.3 KB catalogue page, tiktoken `o200k_base`)

| tool | tokens | bytes | vs cu |
| --- | ---: | ---: | ---: |
| **cu** `snapshot` | **350** | 931 | 1× |
| agent-browser `snapshot -i` | 480 | 1 148 | 1.4× |
| agent-browser `snapshot` | 953 | — | 2.7× |
| Playwright MCP `browser_snapshot` | 1 314 | 3 684 | 3.8× |

### Resources and concurrency: 50 tests, 50 tabs, one Chrome

One `cu` daemon, one Chromium, one leased tab per test (flow = open tab →
navigate → snapshot → act by ref → judge text → close). Machine: 12 cores,
64 GB, already carrying other jobs.

| concurrency | tests/s | flow p50 | flow p95 | Chrome CPU (cores) | Chrome RSS | cu RSS |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 5 | 0.15 \* | 0.15 s | 0.17 s | 0.05 | 0.8 GB | 2.1 MB |
| 10 | 12.4 | 0.29 s | 0.30 s | 0.12 | 1.0 GB | 2.8 MB |
| 20 | 18.9 | 0.52 s | 0.56 s | 0.18 | 1.3 GB | 4.3 MB |
| 35 | 22.7 | 0.96 s | 1.02 s | 0.33 | 1.5 GB | 8.1 MB |
| **50** | **24.5** | 1.40 s | 1.50 s | 0.35 | 1.7 GB | 10.6 MB |

**Headline:** 50/50 tests passed at 50-way concurrency in 2.1 s wall, peak
**1.84 GB Chrome RSS** across 11 processes (≈ **18 MB per open tab**),
**0.31–0.35 of 12 cores** of Chrome CPU and 11 MB for the `cu` daemon. All 100
tests of the 20- and 50-concurrency waves passed with zero errors.

- **Chrome is not the bottleneck**: < 0.4 cores and 1.8 GB of 64 GB at 50 tabs.
- **Throughput flattens around 25–35 concurrent flows** (12 → 19 → 23 → 24
  tests/s) while per-flow latency grows ~linearly (p50 0.29 s → 1.40 s from 10
  to 50). The serialized loop-back round trips and process spawns saturate
  first, not the renderer.
- **Recommended:** 8 concurrent tests by default, 12 as the interactive cap,
  up to 50 for unattended batches. Leases make the pile-up self-cleaning.

\* The 5-concurrency wave hit one cold-start outlier (a single call hung to
the harness timeout); every later level ran with 0 errors. Reported as measured.

### Against a real production site

**Pending.** A measurement of `cu` against a real production web app
(signed-in flows over the live site) is running separately; its numbers will
be added here when the run finishes. Nothing is invented in the meantime.

## Agent surface

```sh
cu start [--data DIR]        # one daemon per machine; a second start fails cleanly
cu status [--short]          # daemon, browser and how many leases are held
cu tab open [URL] [--lease S] [--label NAME]   # one tab per test
cu tab close ID · cu tab renew ID [SECONDS] · cu tabs [close ID]
cu context open NAME [--lease S] · cu context close NAME   # isolated cookie jar
cu lease [KEY [SECONDS]]     # list held leases / extend one
cu navigate URL · cu snapshot · cu text        # all take --tab ID / --context NAME
cu click e3 · cu type e2 "text" [--submit]
cu act '[{"do":"type","ref":"e2","text":"Ada"},{"do":"click","ref":"e5"}]'
cu batch 'snapshot --tab t1' 'click e3 --tab t1'   # many commands, one call
cu shot [FILE.jpg] · cu downloads · cu login
cu session save NAME · cu session load NAME
```

Everything answers compact JSON, one line per command for `cu batch`, and
errors say what is wrong and what to do. Underneath: `GET /v1/status`,
`/v1/snapshot`, `/v1/text`, `/v1/tabs`, `/v1/downloads`, `/v1/leases`,
`POST /v1/navigate`, `/v1/click`, `/v1/type`, `/v1/act`, `/v1/lease` and
session routes — all bearer-token protected, loopback only. The Rust SDK
(`Client`) mirrors the CLI.

## Security

The daemon binds to loopback only. Its bearer token is generated per data
directory and stored in `.cu/server.json`, which is gitignored and must never be
committed or published. Credentials are typed into the page by the daemon from
the local login form; they are never passed to the model, logged, or returned.

## Reproducing the benchmarks

```sh
python3 bench/cu_bench.py --iters 20    # cu before/after, end to end
python3 bench/compare.py --iters 10     # cu vs agent-browser vs Playwright MCP
```

Both harnesses are described in [docs/BENCHMARKS.md](docs/BENCHMARKS.md),
together with the methods, the fair-play notes and every number above.
