---
name: cu
description: >
  Drive one always-open Chrome through the cu daemon: open a leased tab or an
  isolated context per test, navigate, read compact ref-addressable snapshots,
  click/type by ref, batch actions, and let leases clean up tabs when a test
  dies. Use for agentic web testing (URL + credentials + goal), user-story
  checks, smoke tests of a deploy, or any browser automation; says $cu.
metadata:
  short-description: "One Chrome, one leased tab per test: agentic web tests with the cu CLI"
---

# cu: agentic web testing against one always-open Chrome

`cu` is a loopback daemon plus CLI that keeps **one Chromium open** (the
singleton daemon in `~/.local/state/cu`; `cu` finds it from any directory).
Every test -- even from another session or project -- gets **its own tab or
context in that same Chrome** and closes it when the test ends. A **lease**
(default 10 min) makes the daemon close the tab itself when the test never
ends: failure, cutoff or a dead agent leave nothing behind. No MCP server: the
CLI is the tool surface, called straight from bash.

## When to use

- Agentic web tests: give a URL, credentials and a goal; verify the outcome.
- User-story checks ("as a user, I can sign up and see the dashboard").
- Smoke tests of a deploy, regression checks after a fix.
- Anything needing a persistent login (`cu session save/load`) across steps.

Not for pixel-visual QA (`cu shot` exists but snapshots are the cheap read) or
sites you must not touch (a test runs real actions; use read-only goals there).

## Resource model (read this first)

1. **One Chrome is always open.** `cu start` (once) boots the singleton daemon;
   every later `cu` call just finds it. Never start one Chrome per test.
   On this server start it with `CU_BROWSER=/opt/google/chrome/chrome cu start`
   (snap chromium cannot write the hidden `~/.local` data dir; `cu start` says
   so instead of failing with exit 21).
2. **One tab (or context) per test.** `cu tab open URL --lease 300 --label my-test`
   returns a tab id; run everything with `--tab ID`. Need cookie isolation
   (parallel logins, per-test accounts)? Use `cu context open NAME --lease 300`
   and `--context NAME` instead: same Chrome, separate cookie jar.
3. **Close at the end, always**: `cu tab close ID` / `cu context close NAME`
   in the success path and in the failure path. The lease is the backstop: if
   the process dies or is cut off, the daemon closes the tab when the lease
   expires. Renew long tests with `cu tab renew ID 600`. Budget a lease of
   (expected test duration + 2 min).
4. **Concurrency**: tabs share Chrome's processes. On this server (12 cores,
   64 GB, loaded) stay at **8 concurrent tabs** (12 absolute max); see
   `docs/BENCHMARKS.md` in the cu repo for the measurements.

## Commands

```bash
cu start                      # once per machine (singleton daemon + Chrome)
cu status [--short]           # daemon + browser + how many leases are held
cu tab open [URL] [--lease S] [--label NAME]   # a tab for this test
cu tab close ID | cu tab renew ID [SECONDS]
cu context open NAME [--lease S] [--label NAME]  # isolated cookie jar
cu context close NAME
cu lease [KEY [SECONDS]]      # list held leases / extend one
cu navigate URL               # page commands take --tab ID or --context NAME
cu snapshot                   # compact page: ref-addressable elements, ~100s of bytes
cu text                       # visible words: prose, API responses, messages
cu click e3 | cu type e2 "text" [--submit]
cu act '[{"do":"type","ref":"e2","text":"Ada"},{"do":"click","ref":"e5"}]'
cu shot [FILE.jpg]            # pixels, only when a snapshot cannot answer
cu batch 'snapshot --tab t1' 'click e3 --tab t1'   # many commands, one call
cu tabs [close ID]            # popups and extra tabs; default tab is flagged
cu downloads | cu session save NAME | cu session load NAME
```

Output is compact JSON everywhere; `cu batch` prints one line per command
(`{"cmd":...,"ok":true,"out":...}` / `{"cmd":...,"ok":false,"error":...}`).
Errors say what is wrong and what to do (`cu: no such tab; see GET /v1/tabs`).
A `"settled":false` on navigate means the page was still loading when the wait
budget ended: take a snapshot before acting. A stale ref means "take a new
snapshot".

## Credentials

Never write a password into a command line, file or message. Run `cu login`,
open the printed `/login` page in the test account's real browser, and let the
daemon type what is entered. For test accounts, keep credentials in
`~/.config/mini-tui/e2e-secrets.env` (`chmod 600`, `NAME=value`) and read them
into variables without printing. Sessions keep the login:
`cu session save run-42` once; `cu session load run-42` before the next run.

## Agentic test pattern: URL + credentials + goal

One test = one tab + one goal + one judged outcome:

```bash
T=$(cu tab open "$URL" --lease 300 --label "story-17 checkout" | jq -r .tab)
cu batch "login --tab $T" "navigate $URL/cart --tab $T" || true
cu navigate "$URL/checkout" --tab $T
SNAP=$(cu snapshot --tab $T)          # read refs, decide, act
cu act '[{"do":"type","ref":"e4","text":"4242 4242 4242 4242"},{"do":"click","ref":"e9"}]' --tab $T
cu text --tab $T | grep -qi "thank you" && echo PASS || echo FAIL
cu tab close $T                       # always, PASS or FAIL
```

Rules for goal steps: one user-level goal per step; follow every action with a
judged check (`assert`-style read of `cu text`/snapshot); prefer exact refs over
guesswork -- refs come from the snapshot you just took, never from memory.

## User stories

Turn a story into a test with the template **given/when/then = setup tab /
goal with refs / judged outcome**:

> As a user, I can reset my password and sign in with the new one.

```bash
T=$(cu tab open "$URL/login" --lease 300 --label "story-reset-password" | jq -r .tab)
# when: drive the flow, one goal per step, refs from live snapshots
# then: judge from cu text / snapshot — the user-visible outcome, not internals
# report + close (below)
```

Independent tests: each has its own tab/context and its own label; never let
one test depend on another's data.

## Reading snapshots by ref

`cu snapshot` returns compact lines like `e3 button "Add to cart"` (plus
`- frame: <url>` sections whose refs work like any other). Address elements by
ref: `cu click e3`, `cu type e2 "text"`. Take a fresh snapshot after any DOM
change; refs are valid for the snapshot that produced them only.

## Failure report (always report failures this way)

```
FAIL <label> (<url>)
- goal:          <the user-level goal>
- expected:      <judged outcome>
- actual:        <what cu text / snapshot showed>
- last refs:     e4 input "Card number", e9 button "Pay"   (from the failing snapshot)
- evidence:      /tmp/fail-<label>.jpg  (cu shot, only if pixels help)
- cleanup:       tab t3 closed (or: lease 300s left to expire — the daemon closes it)
```

Include what ran (URL, environment), pass/fail per story with the reason, and
what you changed. Never paste secrets or cookies.

## Housekeeping

- `cu lease` lists open tabs/contexts and their remaining seconds: a non-empty
  list after a run means a test leaked; it will still be reaped by its lease.
- Downloaded files land in the daemon's data dir; list them with `cu downloads`.
- The bearer token is in `<data>/server.json` (`~/.local/state/cu` by default);
  keep it private. The server binds to loopback only.
