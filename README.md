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
**0.31–0.35 of 12 cores** of Chrome CPU and 11 MB for the `cu` daemon. The
50-way level ran 100/100 green with zero errors.

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

Everything above runs against local pages. This section runs the flows a QA
agent actually runs against **a real production web app in a development build**
(Vite dev server: one request per module, the browser allows ~6 sockets per
origin), over the network, with a signed-in session shared by the flows of a
level and one leased tab per flow in the one shared Chrome. Every number is
p50/p95 over the flows of its level, measured; the box is a shared 12-core
machine (load1 5-11 during the runs), so absolute times include that noise.

The flow itself: open a tab on a deep route -> sign in if the app says there is
no session -> navigate a section and a sub-route -> open the assistant's chat
drawer, start a new conversation, send a marker and wait until that marker shows
**twice** in the page text (the echo of the message plus the streamed answer) ->
close the tab. No timing sleeps: every wait is on text, state or URL.

#### 1, 10 and 25 flows in parallel (shared session)

| flows | ok | total p50 | total p95 | answer p50 | share of flow | Chrome cores (avg) | Chrome RSS | `cu` cores |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 4 / 5 | 22.1 s | 25.1 s | 6.9 s | 31% | 0.15 | 5.7 GB | 0.01 |
| **10** | **18 / 20** | **67.4 s** | 80.1 s | 6.5 s | 12% | 0.25 | 6.3 GB | 0.03 |
| 25 | 3 / 25 | 48.2 s | 74.0 s | 5.2 s | 11% | 0.28 | 6.5 GB | 0.03 |

- **Recommended: up to 10 flows per shared Chrome** against this deployment --
  ~90% success, per-flow p50 ~67 s of which ~6.5 s is the assistant's answer, and
  Chrome well under a third of a core. Expect p95 ~80 s and one retry when "the
  app never rendered".
- **At 25 flows only 3 of 25 finished, and that is the app's limit, not `cu`'s.**
  The other 22 sat in the empty app shell for 60 s without rendering, reproduced
  twice: a dev build cold-boots one request per module, so 25 cold boots queue
  into minutes. The same 25 leased tabs against the local pages above were
  100/100 green. To go wider: stagger the first navigation (~1-2 s apart), use a
  production build of the app, or split across browsers/origins.
- **Chrome is not the bottleneck.** 0.15-0.28 cores of the 12, and the ~6 GB RSS
  is the *whole* shared browser (other agents' tabs included) -- per-tab cost
  stays ~18 MB as in the sweep above. The daemon is 0.01-0.03 cores and ~2 MB.
  The bottleneck is the app server and its per-origin socket budget.
- **Sign-in is cheap when the bot-check is happy and brutal when it is not.**
  Cold logins ran 0.75-1.5 s each; under throttling (this box's IP also scrapes
  Google) the same login took 30-120 s, and parallel cold logins stepped on each
  other (8 sessions dropped to the login screen mid-flow at the 25 level;
  agent-browser completed 0/20 in 120 s that way). Stagger real logins; share
  one session for concurrency runs.

#### The answer is the one phase that does not queue

The page in front of the model degrades with concurrency; the model backend does
not: ~5-7 s p50 at 1, 10 and 25 flows alike (p95 ~20 s at 10). Its share of the
flow moves the other way -- 31% with one flow, ~12% at 10-25 -- because
page-side contention grows while the answer does not. Timeouts of 60-120 s
cover the p95.

#### Levers, measured at the 10-flow level

Waiting by **text** instead of `networkidle` is not an optimisation, it is the
only thing that works. The app holds two event streams open for its whole life
(a live list and a conversations stream), so `networkidle` is never reached:

| wait | measured |
| --- | ---: |
| `navigate` returns (smart DOM settle) | 2.2 s |
| `networkidle` after that navigate | **never** |
| answer visible by page text, after send | 11.7 s |
| `networkidle` after the send | **never** |

Blocking images/fonts/media over CDP (`cu` has no switch for it yet) moves
almost nothing on this app -- the weight is JS modules and the socket queue, not
media:

| run | ok | total p50 | total p95 | answer p50 |
| --- | ---: | ---: | ---: | ---: |
| baseline | 18 / 20 | 67.4 s | 80.1 s | 6.5 s |
| images/fonts/media blocked | 19 / 20 | 65.5 s | 76.2 s | 4.7 s |

Incremental snapshots do not exist (`/v1/snapshot` is rebuilt whole every call)
and would not pay on a page like this: a warm snapshot of the rendered view is
2-4 ms and ~2.2 KB, byte-identical across calls because the refs are stable. A
delta would save bytes, not time, at this page size.

#### The same flows through agent-browser (10 flows)

| run | ok | total p50 | total p95 | open p50 | answer p50 | browser procs | RSS max |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `cu`, one shared Chrome | 18 / 20 | 67.4 s | 80.1 s | 0.1 s | 6.5 s | 27 | 6.3 GB |
| agent-browser, one browser per session | 18 / 20 | 58.6 s | 69.7 s | 12.0 s | 9.2 s | 113 | 12.8 GB |

Totals are comparable because at 10 flows each tool spends the time somewhere
else: `cu` queues on the app's per-origin socket budget but its per-step calls
stay fast (open 0.1 s), while ten agent-browser sessions cold-boot the app with
no shared cache (open 12 s, 30 s of app boot each) but get ten independent
socket budgets. Both wait by text; the answer is 6.5 s (`cu`, text poll) vs 9.2 s
(agent-browser, coarser snapshot poll). Two flows of each run were lost to the
same app-boot flake. The shape is architectural: one Chrome for every tab (27
processes at 10 flows) against a full browser per session (113 processes, twice
the RAM).

#### What these runs say about `cu` itself

- **A page command sent across a client-side navigation can hang for ever.** The
  daemon waits with no deadline and leaks a thread and a connection each time;
  the harness retried and finished. A deadline per CDP command belongs in `cu`.
- **Refs die when the app re-renders between snapshot and click** ("could not
  locate eN"): re-snapshot and retry; typing in the composer also kills the send
  button's ref, so send with type + Enter.
- **`cu act` exits 0 with an `{"ok":false}` body** -- callers must read the body.

## Parallel agents

The sweep above runs scripted flows. This one runs **real AI agents**: each
"agent" is a headless `mini-tui` session driven by
MiniMax-M3.1-Flash-Preview, given the exact `cu` commands and left to decide the
flow itself -- open **its own** `cu` context (isolated cookie jar inside the one
shared Chrome), navigate, sign in if the page asks, open the app's assistant
chat, start a new conversation, send a message with a unique marker, wait for
the answer **by text** (the marker shows up twice: the echo plus the streamed
reply), judge it, close its context. So this measures model latency, one bash
round-trip per step and a whole agent session on top of the browser, twice.

Target: the **Sage chat of neural-dev**, a production app, static build served
behind nginx/Cloudflare. Box: 12 cores, 64 GB, 8 GB of swap already consumed
before the run (pressure not caused by this benchmark). One shared Chrome for
every agent; `MemAvailable`, swap, shared-Chrome RSS and agent RSS sampled every
5 s, with a 3 GB hard floor that would have killed only this run's PIDs.
**That floor never came close.**

### Round 1 -- 1 to 12 agents

| agents | ok | success | p50 | p95 | min available RAM | Chrome RSS max | agents RSS max |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 1 / 1 | 100% | 117.6 s | -- | 24 700 MB | 1.8 GB | 64 MB |
| 2 | 2 / 2 | 100% | 162.4 s | 162.4 s | 24 494 MB | 2.2 GB | 129 MB |
| 4 | 4 / 4 | 100% | 136.9 s | 136.9 s | 24 137 MB | 2.7 GB | 248 MB |
| 6 | 6 / 6 | 100% | 75.3 s | 149.6 s | 23 729 MB | 3.4 GB | 360 MB |
| 8 | 8 / 8 | 100% | 162.6 s | 162.6 s | 23 311 MB | 4.1 GB | 477 MB |
| 10 | 9 / 10 | 90% | 136.8 s | 244.4 s | 22 712 MB | 4.6 GB | 593 MB |
| 12 | 11 / 12 | 91.7% | 162.6 s | 270.9 s | 22 280 MB | 5.3 GB | 711 MB |

(The 1-6 levels came from a first sweep whose stopwatch collected finished
agents serially, so its latencies are floors, not medians; pass/fail and RAM are
valid in every row. The 10 and 12 rows above are the corrected repeat, one
timing thread per agent.)

### Round 2 -- 8 to 16 agents

| agents | ok | success | p50 | p95 | min available RAM | Chrome RSS | agents RSS | 429 events / agents hit / fatal |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 8 | 8 / 8 | 100% | 121.7 s | 271.0 s | 22 516 MB | 3.6 GB | 475 MB | 0 / 0 / 0 |
| 10 | 10 / 10 | 100% | 76.3 s | 261.9 s | 23 031 MB | 4.2 GB | 602 MB | 0 / 0 / 0 |
| 12 | 12 / 12 | 100% | 125.7 s | 200.3 s | 22 747 MB | 4.7 GB | 717 MB | 0 / 0 / 0 |
| 12 (repeat) | 11 / 12 | 91.7% | 125.4 s | 202.5 s | 20 261 MB | 4.7 GB | 706 MB | 3 / 2 / 1 |
| 14 | 12 / 14 | 85.7% | 180.7 s | 374.3 s | 22 063 MB | 5.1 GB | 817 MB | 8 / 2 / 2 |
| 16 | 12 / 16 | 75.0% | 180.7 s | 368.5 s | 21 939 MB | 5.6 GB | 939 MB | 7 / 3 / 3 |

Round 2 ran because the model's token plan had reset. It cut the 429s at 8 and
10 to zero and lifted success at the already-measured levels (10: 90-100% ->
100%, 12: 83-92% -> 100% / 91.7%) -- but the same 429 came back at 12b, 14 and
16. A reset widens the margin; it does not remove the ceiling. The hard stop
rule fired at 16 (`success rate 75.0% < 80.0% at level 16`); the RAM condition
never came near firing.

### What failed, and what did not

Every failure in both rounds except one was the **same error, returned by the
app inside the conversation**:

```
Error code: 429 - rate_limit_error: All credentials for model
claude-minimax-m2-5-highspeed are cooling down via provider claude
```

Round 1: 3 failures, all 429. Round 2: 7 failures, 6 of them 429 and 1 agent
that interrupted itself before its own 180 s window closed (counted as a fail by
the harness, but it is the agent's decision, not capacity). Classified: **6
provider 429s, 1 agent self-interruption, 0 caused by the UI, the login, `cu` or
the browser**. The navigation, sign-in, new conversation and marker worked every
single time. What runs out is the app's LLM gateway credentials under
concentration, and that is not fixable from the browser side. Fatal 429s grow
with the level: 0, 0, 0, 1, 2, 3 at 8, 10, 12, 12b, 14, 16.

**RAM was never the limit.** Available memory never dropped below 20.2 GB
(round 2) or 21.9 GB (round 1) -- more than 6x the 3 GB floor -- and swap delta
was ~0 at every level. Where the memory goes, measured: the shared Chrome grows
~300 MB per open context (18 -> 28 -> 33 browser processes, ~1.8 GB at rest to
~5.6 GB at 16 contexts) and dominates; each agent process is flat at 15-60 MB
(16 agents = 939 MB). Nothing grew superlineally: doubling agents costs ~1.6x
the Chrome's RAM.

### Recommendation

- **8-10 parallel agents: safe.** 100% twice at each level, zero 429s in round
  2, p50 76-122 s, RAM to spare.
- **12: the limit.** 100% once, 91.7% on the repeat (one fatal 429). Fine if
  you accept retrying the 429s; p95 ~200 s, under the 2x reference threshold.
- **14+: not recommended** with this provider. 85.7% and 75.0%, 2-3 fatal 429s
  each, p95 368-374 s (>2x the level-1 p95 of 117.6 s).
- **Beyond ~12: stagger the launches** (waves of 8-10) or raise the app's
  gateway credential/model concurrency. RAM is not what stops you.
- **Per-agent timeout: 420 s** covers the worst p95 observed (~374 s) with
  margin; 300 s stops being enough at 14-16.

**Marginal cost per agent: ~0.3 GB** -- ~0.25 GB in the shared Chrome (one
context, ~300 MB) plus ~0.06 GB for the agent process itself. An AI agent is
cheap in RAM here; a Chrome context is not.

### Measured vs extrapolated

Everything in the three tables above is measured, per level, on those runs. What
is **not** measured, stated plainly:

- A linear extrapolation of ~0.3 GB per agent says 64 GB would hold many times
  this. That is arithmetic, not evidence, and it is not a claim this page makes.
- **CPU and shared-Chrome contention were never isolated from 429 retries.**
  Agents that hit a 429 retry, and a retrying agent is not idle, so the latency
  column mixes provider backoff with local contention. Nothing above 16 agents
  was measured, and 16 already failed the 80% rule.
- The `seconds=` each agent reports is its own estimate of Sage's thinking time
  and is inconsistent (3 s, 132 s and 27 s in the same level), so it is recorded
  but never used as a metric. Latency here is the harness's own wall clock.
- At n <= 16 the harness's "p95" is always the maximum (int(0.95n) = n-1), so
  "p95" reads as "the slowest agent", and it depends more on one agent's luck
  than on concurrency. It was logged as a flag, not a stop rule, for that
  reason; the stop rule was success < 80%.
- The MiniMax gateway returns no usage: `prompt_tokens` and `completion_tokens`
  are 0 in every trajectory and `cost_usd` is 0.0 in every run, so **token cost
  is not measurable with this configuration** and no number is invented here.
  What is measured instead: ~14-16 tool steps and ~15-16 model calls per agent.

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
together with the methods, the fair-play notes and every number above. The
real-site section reports a run whose harness is not published (it drives a
private app): the method and every number are written out in the section. The
parallel-agents section is measured the same way: two rounds of real agent
sessions against a private app, with the method, the sampling, the stop rules
and every number written out in the section, including what was **not** isolated
from the runs.
