# Changelog

## [1.13.1] - 2026-11-10

### Changed

- **The MCP instructions say what to do when a connection is shared.** A
  harness that runs one MCP client for all its sessions and subagents gives
  them all one cu session, so one "session tab": a call without `tab` lands
  on whichever page was opened last, another agent's included. The way
  around it was already there (`new_tab: true`, then `tab` on every call,
  which also takes the call out of the session's queue); the instructions
  and the note that names a fresh tab now say so.

## [1.13.0] - 2026-11-10

The MCP server, as a QA agent uses it. What an agent read from `cu mcp` was
measured by speaking JSON-RPC to it: `navigate` answered 803 characters of
DevTools ids, timings and a "ready" challenge and no page, so every
navigation was two calls; running a script or waiting for the page meant
writing an `act` batch by hand and getting a whole snapshot back with the
value; `status` said `chromium` and nothing else; `tabs` listed everybody's
tabs alike.

### Added

- **MCP tools `wait` and `eval`.** `wait` takes one of `text`, `selector`,
  `url` (`gone`, `timeout`, or `ms` for a plain pause) and answers "there
  after 420 ms" and the snapshot; `eval` takes `js` and answers the value
  itself, with a script that throws as the tool's error. Both are the `act`
  steps of 1.12, first-class.
- **`eval` answers any value.** `cu eval '1+1'` answered nothing: only strings
  came back, so every count, flag and object needed a `String(..)` around
  it. A string is still answered as it is; anything else as JSON (`3`,
  `true`, `null`, `{"rows":40}`), `undefined` as no value.
- **`navigate` answers the page.** Over MCP it asks for the snapshot by
  default (`snapshot: false` to skip it) and drops what only cu needs from
  the reply: status, `settled`, the `console` problems of the load and the
  snapshot in 632 characters, where 803 carried no page. On the HTTP API,
  `"snapshot":true` in the body of `POST /v1/navigate`.
- **`status` says what is running.** `GET /v1/status?detail=1`
  (`cu status --detail`, the MCP `status` tool): cu's version, the browser's
  engine (`slimium`, `chromium`, `chrome-headless-shell`), executable and
  product, whether it is headless, how idle tabs are kept and whether the
  frame-rate limit is on. The plain `GET /v1/status` is unchanged.
- **`tabs` says which are yours.** Each tab carries `yours`: whether this
  session opened it (the MCP session's own record; on the CLI, the session
  named in the request). Several agents share one daemon, and a list that
  does not say whose tab is whose is how one closes another's page.

### Changed

- The server's `instructions` (what a client shows its model at connect)
  now say how a QA pass goes: `navigate` answers the page, `wait` instead of
  pausing, read `"console"` in every reply, `net` and `eval` for the rest,
  leave tabs that are not `yours` alone.

## [1.12.0] - 2026-11-10

Less time per QA pass. One real session, measured from its logs: 80 shell
commands carrying 90 cu commands, 31 `sleep`s between them adding up to 162 s,
four `cu console` calls. cu's own time for those commands was a few seconds;
the session's time was the agent's round trips and its guesses at how long a
page needs. This release goes after both.

### Added

- **`wait` for the page, not for a guess.** `{"do":"wait","text":"Saved"}`,
  `"selector":".row"` or `"url":"/done"`, with `"gone":true` for something
  that should go away and `"timeout"` in seconds (default 10, at most 60);
  on the CLI, `cu wait --text T | --selector CSS | --url PART [--gone]
  [--timeout S]`. It looks every 100 ms with the tab in front, returns the
  moment the condition holds (with the page's snapshot, like any action) and
  fails naming what never came. On a page that shows its result 1.5 s after
  a click: 1516 ms, where a `sleep 4` took four seconds and a `sleep 1` came
  back too early. `{"do":"wait","ms":N}` is still a plain pause.
- **Replies say what went wrong.** The reply of `navigate` and of every
  action batch (`act`, `click`, `type`, `wait`) carries
  `"console":{"new":N,"entries":[...]}` when the page logged an error, threw,
  violated its CSP or had a request fail or answer 4xx/5xx since the last
  reply for that tab (the last ten, 300 characters each; one evaluation).
  An error nobody asked about was an error the QA missed. Warnings and log
  lines stay in `cu console`.
- **`cu net`: the API calls the page made.** Every fetch and XHR with its
  method, address, status and duration, newest last (`--grep`, `--failed`,
  `--since`, `--limit`, `--json`; `GET /v1/net`; MCP tool `net`). The same
  wrappers that note failures for `cu console --level network` now keep a
  ring of the last 500 requests per document, so it costs nothing until it
  is asked for. Scripts, styles and images are not listed.

### Changed

- **A hidden tab's timers run at full speed** in fast mode
  (`--disable-background-timer-throttling`, `--disable-renderer-backgrounding`,
  `--disable-backgrounding-occluded-windows`). Chromium wakes a hidden tab's
  timers once a second, and once a minute after five minutes, and the tab an
  agent is acting on is a hidden one: a reaction a page schedules 1.5 s after
  a click arrived after 2.2 s. It still paints nothing; an unused browser
  with three parked tabs costs the same as before (0.8 s of CPU in 90 s).
- **A scroll returns when the page has stopped moving**, not after a fixed
  300 ms: 137 ms instead of 340 ms, the position already final. A scroll on
  an element (`ref`) keeps the fixed wait: what scrolls there is a container
  whose position is not the page's.

## [1.11.0] - 2026-11-10

A resource pass on the daemon as agents use it: several tabs, seconds between
commands, the browser left open. Measured on a QA flow of 38 commands over
three development apps, 8 s apart, headless Chromium 156 with software
rendering (cu-slim on slimium), 1.10.0 with slimium 0.3.1 against 1.11.0 with
slimium 0.3.2: **458 -> 29 CPU-seconds of browser** for the flow, two cores
-> 3 % of one while the agent is quiet, 6.4 -> 4.2 s spent in commands, peak
anonymous memory 1009 -> 501 MB (mean 521 -> 412 MB). Four rounds of the flow
in one browser (152 commands): the same CPU each round, memory flat from the
second round on.

### Changed

- **The frame-rate limit stays unless `CU_FAST_CAPTURE=1`.** From 1.8 to 1.10
  Chromium was started with `--disable-frame-rate-limit`, which makes a
  capture of a tab in front cost ~6 ms instead of ~34 ms and makes a page
  that animates cost the cores it takes to render it (2,700 frames a second,
  two cores, for a dashboard with sixteen pulsing dots). On the flow above,
  with everything else in this release in place: 82.9 CPU-seconds of browser
  without the limit, 27.5 with it, for 3.7 s of commands instead of 4.1 s.
  And captures of a background tab were slower without it, not faster: 1.0 s
  each when they came one after the other, 85 ms with the limit.

### Fixed

- **A screenshot of a background tab took five seconds.** A headless tab that
  is not in front makes no frame, and `Page.captureScreenshot` waits for one:
  on a page that is not animating it answered only after cu's 5 s read timeout
  and a second try. The tab cu opens is a background tab, so this was every
  capture until the first scroll. 25 captures of a static page behind a blank
  tab: 24 took 5.1 s (annotated ones 25 of 25); the same page in front, 61 ms.
  A capture now brings its tab forward first (4 ms; the document already says
  `visible` on the next command, 40 of 40): 75 captures of static and
  animated pages, plain, annotated and at another width, 76-233 ms on
  average, none slow. Captures of one browser are taken one at a time,
  because bringing one tab forward hides the others of its window.
- **An annotated capture of a hidden tab waited a second for nothing.** The
  overlay and the measure step wait for two animation frames with a 250 ms
  timer as the way out. A hidden document gets no animation frame and its
  timers wake once a second, so the way out took about a second
  (`shot --marks --annotate all` on a background tab: 1.1-1.3 s; in front,
  50-66 ms). A document that says it is hidden is no longer waited for.
- **A tab left in front painted for ever.** `scroll`, a tap and the touch
  gestures bring a tab forward and nothing put it back; the first tab of a
  browser context is in front from birth (103 % of a core with no command
  ever sent to it); the browser puts a neighbour in front of a tab that is
  closed. A page with a looping animation is then painted whether or not a
  command is being sent: 16-18 % of a core at 60 frames a second and two
  cores without the frame-rate limit. One QA daemon used 4 d 22 h of CPU in
  two days this way. It also never let the browser be idle, which is the
  only time slimium's memory governor collects.
  The new parker (`src/park.rs`) keeps every headless window behind a blank
  tab (`Target.activateTarget`) unless something needs it in front. What
  brings a tab forward says how long it stays: `CU_PARK_AFTER` seconds
  (default 3) after a scroll or a touch, whose page goes on painting; one
  second after a capture, which is over when it returns (three captures in a
  row stay in front for the run: 823 ms for the three). Every other command
  works on a hidden tab and brings nothing forward. A window is never parked
  while a command runs in it or a person has one of its tabs open in the
  live viewer, and never on a browser with a window. Whenever a page appears
  or goes away, every window is looked at again. A window with no blank tab
  gets one, which `cu tabs` lists; closing it is noticed and another is
  made. The cost: a capture of a tab that was parked repaints it from cold,
  about 110 ms instead of 61 ms, and a page sees `visibilitychange` each
  time it is parked and brought back. `CU_PARK_AFTER=0` is the old behaviour,
  five-second captures of background tabs included.
- **`TRACE cdp ...` was written for every DevTools command**, `CU_TRACE` or
  not, into a daemon log that is only ever appended to (1.1 MB after four
  days of QA). It is behind `CU_TRACE` again, as the documentation says.

## [1.10.0] - 2026-11-06

### Added

- **`CU_PORT`** sets the HTTP port of `cu start` and `cu daemon`, and
  `CU_CDP_PORT` now applies to `cu daemon` too: with `CU_DAEMON_DIR` a second,
  isolated daemon (e.g. `cu-slim`, on slimium's `slimium-chrome`) runs beside
  the canonical one. A daemon auto-started for a `CU_DAEMON_DIR` no longer
  asks systemd for the canonical `cu` unit.
- **JavaScript dialogs are answered, always.** cu had no dialog handling at
  all: Instagram's story editor arms a `beforeunload` handler, and after the
  editor was discarded the next navigation opened a "Leave site?" dialog
  nobody answered. The tab was then stopped -- every command on it, even
  `cu eval document.title`, failed after 5 s with "Resource temporarily
  unavailable (os error 11)", `cu act emulate` too -- and so was every tab
  sharing its renderer process. Now every tab cu works in has a keeper
  session with `Page` enabled whose thread reads the tab's events and answers
  `Page.javascriptDialogOpening` by policy as it opens, between commands too:
  `beforeunload`=accept, `alert`=accept, `confirm`=dismiss, `prompt`=dismiss.
  `manual` leaves a dialog open: commands on the tab fail at once (409) naming
  it, including the one that opened it, until `cu dialog accept [TEXT]` /
  `cu dialog dismiss`. Policy per daemon (`CU_DIALOGS="beforeunload=accept,confirm=manual"`,
  `all=manual`) and per tab (`cu dialog policy SPEC`). Every dialog is logged
  (bounded, message cut to 200 characters): `cu dialog` (`GET /v1/dialog`),
  `cu console --level dialog`, and `"dialogs":[...]` in the reply of the
  action it happened under. A dialog that opened before cu watched a tab
  cannot be answered by any session (Chromium 156, measured); cu says so and
  `cu tabs close` still closes the tab. New routes `GET /v1/dialog`,
  `POST /v1/dialog/accept|dismiss|policy`, MCP tool `dialog`.
  (`src/keeper.rs`, `src/dialogs.rs`)

### Fixed

- **A stopped browser no longer fills up with tabs, and an exited session's
  tabs are collected.** While tabs were stopped by a dialog, repeated calls
  left ~100 extra `about:blank` tabs and several old ones behind. Causes,
  each fixed: a script's `$(cu ...)` (or pipeline) ran cu in a forked
  subshell, which was taken for a session of its own -- a fresh tab per call,
  and with a dialog stopping the shared renderer each of those tabs was
  stopped too; the subshell is now seen through. A failed or cut-off `/json`
  read (`http_get` hands back what arrived on a timeout) was taken for "no
  tabs" and dropped every lease of the session; it is now "unknown" and
  drops nothing. A session pid from another pid namespace (a sandboxed
  agent) was looked up in the daemon's `/proc` -- dead (a fresh tab each
  call) or an unrelated live process (never collected); the CLI and `cu mcp`
  now send `X-Cu-Session-Ns` and such a session keeps its tab, collected
  after ten idle minutes. A pid reused by a later process is no longer its
  predecessor's owner (start time checked). Tabs of exited sessions were
  only collected ten minutes after the last use, by a sweep every minute;
  now after one idle minute, checked every 10 s and before any new tab is
  opened, and even when a dialog stops them (`Target.closeTarget` needs no
  cooperation from the page). Leases are per browser port.

### Changed

- **One keeper session per tab.** Emulation's keeper, the console capture's
  registration connection and the dialog watcher are now the same DevTools
  session (`src/keeper.rs`): one idle socket and one blocked reader thread per
  tab instead of up to two silent connections. `desktop` resets the
  overrides on it and leaves it open.

- **`emulate` is a whole phone, and it stays on.** `{"do":"emulate","mode":"mobile"}`
  set the viewport and touch but left `screen.orientation`
  `landscape-primary`, the screen the desktop's and the User-Agent the
  desktop's, so Instagram's web story editor answered "Rotate your device"
  and its home page a tablet-app upsell. Now a phone (and a tablet) is the
  full set: `screenWidth`/`screenHeight`, `screenOrientation` (portrait, or
  landscape at 90 degrees when wider than tall), device pixel ratio 3 for a
  phone and 2 for a tablet, touch with 5 points, and an Android Chrome
  User-Agent of the running build with matching `userAgentMetadata` (an
  override without it empties `navigator.userAgentData`). `desktop` gives
  back the browser's own window and cu's own User-Agent exactly. New
  optional step fields: `orientation` (`portrait` | `landscape`) and
  `scale`; the step answers what is in force as its `value`. Existing steps
  parse as before. (`src/emulation.rs`, `src/actions.rs`)
- **The emulation survives the page and the daemon's connections.**
  Chromium drops a DevTools session's overrides when its socket closes, and
  the overrides were set on a pooled connection -- which the daemon drops
  when a navigation outlasts the settle budget. A tab now keeps its device on
  a connection of its own, re-applied if that connection dies, so reloads the
  page makes itself and later `snapshot`/`click`/`eval`/`text`/`shot` keep
  it. `cu shot --width/--scale` on an emulated tab puts the whole device back
  afterwards instead of a measured viewport.
- **A click on an emulated touch device is a tap** (`Input.dispatchTouchEvent`:
  `pointerdown` of type `touch`, `touchstart`, then the click), sent after
  bringing the tab to the front; `Emulation.setEmitTouchEventsForMouse` is
  not used.

### Tests

- `tests/emulate_real_browser.sh`: an isolated daemon (ports 8795/9395) on a
  real Chromium and a local fixture that reloads itself after a click.
  The fake browser now drops a connection's overrides when it closes, like
  Chromium.
- `tests/dialog_real_browser.sh`: an isolated daemon (ports 8796/9396) on a
  real Chromium and a local fixture (9598) whose buttons arm `beforeunload`
  and open `alert`/`confirm`/`prompt`, plus a page that shows an alert before
  cu watches it; it also counts tabs across failing calls and times the
  collection of an exited session's tab.
- The fake browser emits `Page.javascriptDialogOpening`/`Closed` to the
  sessions that had `Page` enabled, stops renderer commands (and a fresh
  `Page.enable`) while a dialog is open, accepts `Page.handleJavaScriptDialog`
  only from an eligible session, and closes a target's sessions on
  `Target.closeTarget`. Integration tests: beforeunload on navigate, the
  alert/confirm/prompt policies, manual fast-fail and `cu dialog
  accept|dismiss`, the console's `dialog` level, an unwatched dialog, a
  stopped session never getting another tab, an exited session's stopped tab
  collected, a foreign pid namespace, and `$(cu ...)` subshells sharing a tab.

## [1.9.0] - 2026-11-05

### Changed

- **Chrome/Chromium is the default browser again; Min is opt-in.** Min as the
  default (1.6.0) is reverted to an opt-in (`CU_MIN_DEFAULT=1` or
  `CU_BROWSER`): Electron cannot be driven as fully as Chromium (no
  `Target.createTarget`, no browser contexts, and a view Min never loaded a
  URL into stays detached from its window, so pages such as Figma's editor
  never return a capture). Without the opt-in nothing is auto-installed.
  `cu eval`, `CU_BROWSER_LOG` and the Min support itself stay.

### Added

- **`CU_BROWSER_LOG=<file>`** keeps the browser's own stderr.

## [1.8.0] - 2026-11-05

### Added

- **`cu eval` / the `eval` action: JavaScript in the page.** `{"do":"eval","js":"…"}`
  in a batch (or `cu eval 'js'`, or the script on stdin) runs
  `Runtime.evaluate` with `returnByValue` and returns the value when it is a
  string. It is the escape hatch for what no snapshot ref reaches: injecting
  a bridge, reading a global the page keeps only in JS. It goes through the
  same gate as every other page command (live viewer, challenge, origin
  budget).


## [1.7.0] - 2026-11-05

### Added

- **SEO: `cu seo` and `cu serp`.** Page audits (`seo audit`: status,
  `X-Robots-Tag`, title/description, canonical, hreflang, headings, words,
  alt texts, links, JSON-LD, Open Graph, top terms and per-keyword
  placement and density, with an `issues` list), a polite same-host crawl
  (`seo crawl`: `robots.txt` and `Crawl-delay` obeyed, at most one page a
  second, JSONL per page plus a summary with duplicates and errors) and
  `seo robots`.
- **Locations per context (`seo geo`).** Country, region and town (looked up
  once with Nominatim, cached) or a given position, applied to a named
  context as one coherent set: locale and `Accept-Language`, time zone,
  `navigator.geolocation` with the permission granted, and an optional proxy
  -- `Target.createBrowserContext` now takes one per context. Overrides are
  re-armed when the browser drops the session that held them. Warnings say
  what is missing (no proxy, an estimated time zone).
- **Location-aware result pages (`serp`, `serp compare`).** Google (`hl`,
  `gl`, `uule`, `pws=0`), Bing (`cc`, `mkt`, `setlang`) and DuckDuckGo
  (`kl`): organic results (Google's `/goto` links resolved from the result's
  own URL line), ads, local pack, related questions and searches, and a
  target domain's position; `compare` diffs several places. Spaced per
  engine (`CU_SERP_INTERVAL_MS`).
- **Google's `/sorry/` page is `human_required`** (vendor `google-sorry`):
  a search that meets it stops with the hand-off instead of reading it as
  results.

## [1.6.0] - 2026-11-05

### Added

- **Min Browser is the default browser.** cu prefers Min (Electron, same
  DevTools protocol) over the Chrome/Chromium candidates, and installs it
  when it is missing: the official amd64 `.deb` from GitHub is unpacked into
  `~/.local/opt/min`, no root needed (`CU_MIN_AUTOINSTALL=0` opts out).
  `CU_MIN_DEFAULT=0` keeps cu on Chrome/Chromium; `CU_BROWSER` still wins.
- **Session tabs on an Electron browser.** Electron refuses
  `Target.createTarget` (`-32000 Not supported`), so on Min a session tab is
  opened through Min's own window UI, the way a link asking for a new tab is,
  and closed with `window.close()` so Min's tab list stays in step. Min's UI
  and internal pages are never listed as tabs or driven.
- **Captures bring a Min tab to the front.** A Min tab that is not shown
  paints nothing and its screenshot never returned; screenshots and the live
  stream now select the tab first.
- **A named context is a tab of its own on Min.** Electron has no
  `Target.createBrowserContext`, so on Min a context is a tab remembered by
  name: the SEO commands (`cu seo audit|crawl|robots|serp`, which always run
  in a context) and `--context` work there without landing in the session's
  page. Cookies are shared with the other tabs, and a proxy or a location
  (`cu seo geo`, `--geo`) is refused with a message instead of silently
  dropped: both ride on a real browser context.

### Changed

- Min always runs windowed (Electron has no headless mode): on `CU_DISPLAY`
  or a private Xvfb, the inherited `DISPLAY` only as a last resort, and with
  its own profile, `profiles/min`. Named contexts need Chrome/Chromium.

## [1.5.2] - 2026-10-29

### Changed

- **Agents may type credentials.** `cu mcp` told the model never to type a
  password or a one-time code and to send `login_url` instead, which blocked
  every login with a test account or a secret the person had handed over. The
  MCP instructions and the `type` / `login_url` descriptions now say `type`
  and `act` fill those fields like any other text, and `login_url` is for when
  the person should type their own. The skill, README and agent notes say the
  same.

### Fixed

- **The snapshot never names a field by a typed secret.** An input with no
  label, placeholder or text was named by its value, so an unlabelled password
  field came back in the snapshot with the password in it. Password and
  `one-time-code` fields are no longer named by their value.

## [1.5.1] - 2026-10-26

### Fixed

- **`cu mcp` keeps its page when it runs in a container.** The daemon resolves
  a session's tab by the caller's pid, read in its own `/proc`; from another
  pid namespace (mini-tui's container shares the host's network, not its pids)
  that pid is a different process or none, so every call landed in a fresh tab:
  `navigate`, then the `snapshot` of a blank page. The MCP server now pins the
  tab the daemon names on the first page call (and on a `new_tab`) and sends
  it explicitly (`?tab=`) on every later call. A pinned tab that is gone
  (closed by hand, collected, crashed) is dropped and the call falls back to
  the session, which opens a fresh tab and reports it.

## [1.5.0] - 2026-10-26

### Added

- **`cu mcp`: the daemon's tools for any MCP client.** A stdio Model Context
  Protocol server (JSON-RPC 2.0, protocol revisions 2024-11-05 to 2025-11-25)
  that pi, Claude Code, Codex, Cursor or Claude Desktop launch as
  `cu mcp` (`pi mcp add cu -- cu mcp`). 21 tools mirror the CLI: `navigate`,
  `snapshot`, `text`, `click`, `type`, `act`, `screenshot` (an image block,
  optionally saved to a file), `tabs`, `close_tab`, `close_context`,
  `console`, `inspect`, `mark`, `marks`, `upload`, `downloads`, `handoff`
  (with `wait` and `release`), `session_save`, `session_load`, `login_url`
  and `status`. Each is one request to the running daemon on the CLI's
  routes, and the daemon is started the CLI's way when nothing answers.
  - One connection, one page: the `cu mcp` process is the session the daemon
    leases a tab to, so `navigate` then `snapshot` read the same page and two
    connections never share one. Calls on that page run in arrival order;
    calls naming a `tab` or a `context`, and a hand-off `wait`, run alongside
    them. On disconnect the pending answers are sent and the session's tabs
    are closed.
  - Upload paths, in the `upload` tool and in `act` upload steps, resolve
    against the server's working directory, not the daemon's.
  - Replies that carry a snapshot answer the rest as JSON on the first line
    and the snapshot as plain lines below it; `text` answers the words. A
    failed call is an `isError` result with the daemon's reason. Tools carry
    read-only / destructive / open-world annotations for permission gates.
  - No tool takes or returns a password, a vault secret or a one-time code:
    `login_url` points the person at the local form, and the vault and
    `cu otp` stay on the CLI.

### Changed

- A daemon the CLI starts on demand no longer inherits the caller's stdin
  (under `cu mcp` that is the client's protocol pipe).

## [1.4.0] - 2026-10-20

### Added

- **One-time codes without a takeover: `cu otp`.** A 2FA page no longer means
  handing the whole browser to a person.
  - `cu otp code DOMAIN` (or `--credential ID`) mints the current TOTP code
    from a seed the vault holds — RFC 6238 over SHA-1/256/512, 4-8 digits,
    any period, parsed from the full `otpauth://` URI or a bare base32 key.
  - `cu otp request DOMAIN [--kind code|setup] [--wait]` opens a hand-off
    and prints a short `/vault/otp?t=…` link: a temporary page, one token
    per request, where the person types the code (or, for setup, scans the
    same QR the agent captured). The agent's `--wait` parks until the answer
    lands, the link expires, or the budget runs out — the navigation resumes
    by itself, no polling loop.
  - `cu otp capture --domain D` reads the setup QR off the page itself:
    square QR-looking elements first (attribute text mentioning qr/otp/2fa
    ranks highest), the full viewport as fallback, decoded in pure Rust
    (`rqrr`). The URI is sealed into the domain's credential as a secret
    `totp` field and is never returned, logged or rendered outside the
    hand-off page. The reply names only the issuer and account it was stored
    for. Capture refuses to guess: the tab must actually be on the claimed
    domain, the QR's issuer must match that host, several different setup
    QRs on one page are an error, the capture always names its tab
    explicitly (`--tab`), and a credential that already holds a live seed
    refuses the overwrite: the refusal names the existing setup so the user
    can confirm the replacement first, and only then `--replace` applies
    (the reply says `replaced: true` and names the previous account).
  - The challenge hint on a first-party code page now points here: seed →
    `cu otp code`, no seed → `cu otp request`, live takeover stays the
    fallback.
- **The vault UI wears the Bizvaretto identity.** Light, clean and minimal:
  Manrope (bundled, OFL, served at `/vault/manrope.ttf`), the studio's paper
  and ink palette, pill buttons, calm cards — login, app, live view and the
  new hand-off page share the language. The login honours `?next=` so a
  hand-off link returns to its page after unlocking.
- **The vault shows code hand-offs.** A Codes view lists them (waiting,
  answered, expired, cancelled) and can cancel a pending one; the badge
  counts pending hand-offs like it counts secret requests.

### Security

- Hand-off tokens are 192 bits of randomness; only their SHA-256 is stored,
  so a database read cannot mint answer links. The hand-off page strips its
  token from the address bar on load and every reply carries
  `Referrer-Policy: no-referrer`; UI-only POSTs carrying an Origin header
  from another host get 403. Collecting an answer is a two-step hand-over:
  the first collect claims the row and hands the code over with a one-owner
  collect token; only that token can re-fetch the code after a cut
  connection or acknowledge it, and the stored value is wiped on the ACK -
  or by the reaper when the five-minute collection window closes, never
  re-exposed, never delivered to a second collector (at-most-once to one
  owner with bounded redelivery, not exactly-once). The CLI generates its
  own collect token from the OS CSPRNG, keeps it - and the retrieved code,
  once it lands - in a 0600 state file next to the daemon's data, and sends
  it as a header, never in the URL; a crashed or killed `cu otp wait` is
  resumed by running it again, and transport errors retry inside the
  budget instead of killing the process. The ACK is idempotent, so a lost
  ACK reply is safe to retry; a definitive refusal (the window closed,
  the token is not the owner's) ends the wait instead of retrying forever.
  The five-minute collection window is the vault's delivery retention, not
  the code's validity - a TOTP code lives about thirty seconds and is used
  immediately. State past ten minutes is deleted when a wait next touches
  the collect directory (stale hand-off files and orphaned temp files
  alike), never resumed - but note this is hygiene on use, not a guarantee:
  a crashed wait nobody reruns can leave its 0600 file (expired token,
  dead code) until the next `cu otp` invocation. The
  hand-off page only reveals its QR block once the image has actually
  loaded - a request without a usable setup QR keeps the code-only
  layout, no broken image. Answers are single-use
  (enforced in the answering UPDATE, not in a check that could race),
  sealed at rest, handed to the polling agent exactly once and deleted in
  the same write; expiry is ten minutes by default (1-15 honoured). Rows a
  day past usefulness are reaped; metadata alone stays as the audit trail.
- Answering, looking up or viewing the setup QR of a hand-off is UI-only —
  the bearer token every local agent holds cannot answer its own request,
  exactly as with secret-request approvals. The temporary page also requires
  the vault's UI session, so a forwarded link is not a key by itself.

## [1.3.0] - 2026-10-19

### Added

- **`cu console`: every console level, plus what breaks around it.** Agents
  only saw page text, never the console. `cu console [--level L] [--grep S]
  [--limit N] [--since N] [--clear] [--json]` (`GET /v1/console`) returns
  `debug` (alias `verbose`), `log`, `info`, `trace`, `warn`, `error`, and also
  `exception` (uncaught errors, unhandled rejections), `csp` (violations) and
  `network` (failed sub-resources, failed or 4xx/5xx `fetch`/XHR). `--level`
  takes a list (`warn,error`), `all`, or `LEVEL+` for that level and worse
  (`warn+`). Capture runs in the page, so cu still never sends
  `Runtime.enable`; console methods are wrapped in `Proxy`s and stay native to
  `toString()`. A tab is captured from its first `cu navigate`/`cu console`.
  `CU_CONSOLE=off` disables it.

## [1.2.0] - 2026-10-19

### Added

- **Scroll, touch and device emulation actions for `cu act`.** Agents could not
  scroll or test mobile layouts. New batch actions:
  - `{"do":"scroll","dy":300}` (also `dx`, optional `ref` to scroll over an
    element) sends a mouse wheel; `{"do":"scroll","to":"top|bottom"}` jumps.
  - `{"do":"tap"|"longpress"|"swipe"|"pinch", ...}` synthesise real touch
    gestures (`ref` or `x`,`y`; `swipe` takes `dx`/`dy`, the finger travel in
    px; `pinch` takes `scale`; `longpress` holds >= 800 ms, `ms` overrides).
  - `{"do":"emulate","mode":"mobile|tablet|desktop|custom","width":..,"height":..}`
    sets (or clears) a touch-capable mobile viewport.

## [1.1.3] - 2026-10-17

### Added

- **`cu handoff wait`: the navigation resumes itself, so the agent does not
  have to guess.** A navigation that lands on an interactive challenge
  (Cloudflare managed challenge / Turnstile) is paused and handed to a person.
  Until now the agent's only move was to retry `navigate` and read the same
  `human_required` -- blind retries that burn the session's reputation, which
  is the one thing the anti-bot line exists to prevent. Now the hand-off is
  *observable and awaitable*, and `cu` picks the navigation back up the moment
  the verification resolves.

  - **`GET /v1/handoff`** (`cu handoff`) reports the hand-off state of a tab:
    `waiting` (bool), the `open` hand-off when there is one (tab, redacted
    URL, vendor, `open_ms`, re-checks, whether the tab was brought forward,
    signals), the last `assessment`, the wait `budget` and the pass `history`.
    The agent can tell "still waiting" from "gave up" without probing the
    page at all.

  - **`GET /v1/challenge?wait=S`** (`cu challenge wait --timeout S`, or
    `cu handoff wait`) **parks until the hand-off resolves** instead of
    answering at once. It re-checks with the same gate the watcher uses --
    never interacting with the tab -- and returns the assessment it ended on,
    plus `resolved`, `timed_out`, `waited_ms` and `polls`. It returns as soon
    as the page reads `ready` (the person finished), or when the verdict turns
    into something no person can fix from here (`blocked` / `rate_limited`), so
    the wait never burns its whole budget on a hopeless page. A wait that ends
    with `timed_out` exits non-zero from the CLI, so a script can tell "still
    waiting" from "carry on" without parsing JSON.

  - **Bounded by construction.** The budget a caller asks for is clamped to
    `CU_HANDOFF_WAIT_MAX_S` (default 900 s) whatever the request says; the
    re-checks use a fixed backoff (250 ms, 500 ms, 1 s, 2 s, 4 s and then
    every 4 s) so a wait costs the daemon little; and the number of probes is
    capped (`timeout / 4 s`, at most 240). A long wait is therefore one
    parked daemon thread, not one request per second, and it cannot spin.

  - The challenge URL's tokens are cut to origin and path exactly as before,
    so a wait (or the hand-off route) can never leak the `__cf_chl_tk` the
    challenge page carries.

  Tests: 4 unit tests over the backoff sequence, the probe budget, the wait
  reply (including that no token leaks) and the published ceiling; 4
  integration tests over the fake browser, which gained two interstitial
  scenarios -- `cf://`, which shows a Cloudflare managed challenge and clears
  itself after two probes (the wait must return `resolved` with `ready`), and
  `cfhold://`, the same page that never clears (the wait must end on its
  budget, say `timed_out`, keep the tab paused, and a one-hour request must
  still come back at the ceiling with a bounded probe count). A test also
  asserts the window hand-off really happened: `Page.bringToFront` followed by
  `Target.activateTarget`, both reaching the browser.

### Changed

- **The vault UI session now says what it means, and lasts as long as you
  work.** Three fixes to the cookie that guards `cu-vault.menustudioai.com`
  (`src/vault.rs`, `src/server.rs`):

  - `Secure` is now set. The vault is served over HTTPS and only over HTTPS,
    but the cookie was travelling without the flag, so any accidental
    plain-HTTP request handed the session token to anything on the path.
  - `SameSite` moves from `Strict` to `Lax`. `Strict` withholds the cookie
    from *top-level cross-site navigations* too, so arriving at the vault from
    a link in a mail client or a chat, or through a redirect, dropped the
    session with no explanation and asked for the master password again.
    `Lax` sends the cookie on those top-level GETs and withholds it on every
    cross-site POST/form — and since every state-changing vault route is a
    POST or a DELETE, forgery protection is unchanged.
  - The session slides. It used to die exactly twelve hours after login no
    matter what you did in between, which logged people out mid-afternoon.
    Now every authenticated request moves the deadline, so a session expires
    after a *period of inactivity* and never mid-work.

  The login page grows a **"Remember this session"** checkbox: unticked, the
  twelve hours above; ticked, thirty days, sliding the same way. The flag is
  a server-side property of the session (`ui_sessions` in `vault.db`), never
  a client-side one — the cookie is `HttpOnly`, so nothing in the page (and
  nothing in `localStorage`) can extend a session's life. `GET
  /v1/vault/ui/session` reports whether the current cookie is live so the
  login page can skip the form when it already is.

  UI sessions moved from a daemon-lifetime `HashSet` to a table in the vault
  database, so a daemon restart no longer logs everyone out. Only a SHA-256
  of each token is stored: the token itself is a bearer credential, and a
  copy of `vault.db` must not be a set of live sessions. Changing the master
  password now ends every existing session.

- **The login page is a real page now** (`src/vault_login.html`): labelled
  fields instead of bare placeholders, a light and a dark scheme, a visible
  focus ring, an announced and legible error state, a busy state on submit,
  and 44px touch targets. The inputs are `font-size: max(16px, 1em)` — the
  rule the rest of the project's UI already follows (mini-tui
  `web/src/app.css`) — because below 16px iOS Safari zooms the viewport the
  moment a field takes focus and never zooms back. The app page's inputs get
  the same floor and a focus ring; they had the same zoom trap.

### Security

- With nginx's `auth_basic` removed from the vault vhost (its native browser
  dialog was the thing being replaced), the vault's own session check is the
  only thing between the public internet and every stored cookie,
  credential and pending secret request. That branch in `vault_route` is
  deny-by-default and covered by a test that walks all fourteen data routes
  with no cookie, a junk cookie and an unrelated cookie, asserting `401` on
  each: `/v1/vault/{status,sessions,credentials,requests,passwd}` and every
  approve/deny. Only the three deliberately-open routes answer without a
  session — the two HTML pages (no data in them) and `/v1/vault/ui/{state,
  session}`, which say only whether a master password is set and whether
  *this* cookie is live. Login is still rate-limited to five attempts every
  five minutes.

### Fixed

- **`cu shot` no longer takes another tool's viewport away with it.** A capture
  asked for a size (`--width`/`--height`/`--scale`) or a clip emulates that
  viewport for the shot and then cleared the override unconditionally. That is
  right for the override it set itself and wrong for one that was already in
  force: another tool (a QA harness emulating a phone, a second agent's
  capture) had put the tab at its own size, and `cu` left it at the browser's
  window size instead, silently. Measured on a live tab emulated at 390x844, a
  plain `cu shot --width 800 --height 600` put the page back at 1366x629, so
  every later action in that session measured a layout nobody asked for.

  CDP has no "what override is set?" command, so `cu` now reads the metrics the
  tab is being shown at before the capture and compares them with what the
  browser falls back to after clearing: the same answer means nothing was
  emulated and the clear was the whole job, and a different one means the saved
  metrics were somebody else's and they go back on. The size and the device
  scale factor are recovered exactly; the `mobile` flag and an explicit
  screen size are not observable from the page and are left to the browser.

- **A session keeps its page, so `cu navigate` and `cu snapshot` read the same
  tab.** In daemon mode every page command without `--tab` opened a *fresh* tab,
  so the flow the skill documents — `cu navigate URL`, then `cu snapshot` — read
  `about:blank`: the second command landed in a new, empty page, and so did
  `cu shot`, `cu text` and click-by-ref. Each command also left its tab behind
  (five snapshots opened five tabs, reaped only ten minutes after the process
  that asked for them died). A page command without `--tab` now works in the tab
  its session already has, opening one only the first time: the second command
  reads the page the first navigated to, and no tab is created per command. The
  session is the nearest ancestor that is not a one-shot shell (`sh -c ...`), so
  two agents are two sessions and still never share a page. `--new-tab` asks for
  a page of this call's own, for a test that wants one.
- **`POST /v1/tabs` waits for the browser like every other page route.** It was
  the only page route outside the readiness gate, so a tab request racing
  Chromium's start-up answered `502` and the CLI, which ignored the failure, ran
  the command in whatever page the daemon was showing — one agent reading
  another's tab without a word. The route now waits, and a CLI that still cannot
  get a tab of its own says so on stderr instead of falling through silently.
- **A closed tab is never handed back to its session**, and its lease goes with
  it; a session whose process is gone does not get its old tab either.

## [1.1.2] - 2026-10-15

### Fixed

- **An HTTP-level navigation failure is reported as the refusal it is.** Chromium
  answers `net::ERR_HTTP_RESPONSE_CODE_FAILURE` when the origin returns a status it
  will not render — X/Twitter's 403 to an automated client — and `/v1/navigate`
  returned a bare `502 navigation failed`, hiding a block the agent could act on.
  The tab is left on the browser's own error page, so the navigation is now
  assessed first: a refusal comes back as `200` with `navigated:false`, the
  browser's `error`, `"challenge"` (`state` `blocked`, vendor `chrome-deny`, the
  `status` the network saw, the signals) and the hint; a transport failure
  (`ERR_NAME_NOT_RESOLVED`, `ERR_CONNECTION_REFUSED`) keeps its `502` — nothing
  answered, so there is no page to classify. `crate::scheduler::http_level_failure`
  holds the list, and those errors are never auto-retried. (`src/server.rs`,
  `src/scheduler.rs`)

  Measured on the deployed daemon (`cu.service`, `CU_HEADLESS=1`, Chrome 145):
  `https://x.com/account/begin_password_reset` answered
  `{\"navigated\":false,\"error\":\"navigation failed: net::ERR_HTTP_RESPONSE_CODE_FAILURE\",
  \"challenge\":{\"state\":\"blocked\",\"vendor\":\"chrome-deny\",\"status\":403,...}}`;
  `https://no-such-host-xyz.test/` still answered `502`; `https://example.com/`
  still `ready`; `https://help.x.com/es/forms/account-access/regain-access` still
  `human_required` / `cloudflare` after the `cf-mitigated: challenge` header.

- Chromium's own renderer refusal page (`chrome-error://chromewebdata/`, "Access to
  <site> was denied / HTTP ERROR 403") reads as `blocked` with vendor `chrome-deny`,
  and is checked before the `not an http(s) page -> ready` branch. It is what
  X/Twitter and other origins that refuse the client outright return (measured
  today against https://x.com/login and https://x.com/account/begin_password_reset);
  before this, the refusal page read as ready. It never carries a URL path worth
  keeping, so the replied URL stays the renderer's. (`src/challenge.rs`)

## [1.1.1] - 2026-10-15

- Chromium's own renderer refusal page reads as `blocked` with vendor `chrome-deny`
  (see the 1.1.2 entry above).

### Added

- HTTP basic auth, answered (`src/server.rs`): when the vault holds a
  user/password credential for a domain, `cu navigate` arms CDP
  `Fetch.authRequired` interception scoped to that host and answers a
  401 + `WWW-Authenticate` challenge with the vault's pair
  (`Fetch.continueWithAuth`) — a headless browser has no dialog to offer, and
  Chrome strips URL-embedded credentials from subresource fetches, so pages
  behind nginx-style basic auth used to be unreachable. Every intercepted
  request is continued on the fly without ever stopping to read a command's
  reply inline. The reply gains `"basic_auth":"responded"` and the post-auth
  document status; `"auth":false` in the navigate body turns the responder
  off.

  The navigation's interception alone was not enough, and handing it back
  once the document settled only moved the problem: a credential delivered
  through `Fetch.continueWithAuth` is not reliably put in Chrome's HTTP auth
  cache (measured against a real nginx-fronted origin over TLS: the realm was
  challenged again for the page's very next request), and between an agent's
  commands nobody is reading the tab to answer it. So a tab that navigated
  with a vault pair keeps a responder of its own: a dedicated DevTools
  connection, `Fetch` armed for that host, answering every challenge with the
  pair and continuing every other intercepted request until the tab closes or
  its connection dies. A navigation arms its own interception only after that
  responder has let go.

- Agent secret requests (`src/vault.rs`, `src/main.rs`, `src/vault_app.html`):
  `cu vault request DOMAIN [--reason TEXT] [--from NAME] [--wait]` asks for a
  credential by API; the request sits pending in the vault (15-minute TTL),
  shows up in the vault UI's new Requests panel (badge + polling), and only a
  person there can approve (linking a credential — the only one for the
  domain by default) or deny. Approve/deny refuse the bearer token outright:
  every agent holds the same token, so an agent that could answer its own
  request would make the approval theatre. `GET /v1/vault/requests/{id}`
  returns the credential with its values only once approved;
  `cu vault request --wait` polls and prints that payoff.

## [1.1.0] - 2026-10-13

### Added

- Visual QA references (`src/visual.rs`): `cu inspect REF|--selector CSS`
  (`GET /v1/inspect`) returns a unique selector (piercing open shadow roots
  with `>>>`), test id, landmarks, box, key styles and state, and on dev
  builds the rendering component, its owner chain and source file
  (React `_debugSource`/`_debugStack`, Vue `__file`, Svelte `__svelte_meta`,
  Angular `ng`, inspector-plugin attributes). `cu mark` / `cu marks [--md |
  clear]` (`/v1/marks`) pin QA notes with severity to elements and render
  them as a Markdown report. `cu shot --annotate REFS|all` labels snapshot
  refs on the image and `cu shot --marks` draws the marks numbered like the
  report; overlays live for one capture in a closed shadow root.

- Browser fingerprint hardening (`src/stealth.rs`, on by default, `CU_STEALTH=0`
  to turn it off): the launch now prefers a window on an X display
  (`CU_DISPLAY`, an existing `DISPLAY`, or an Xvfb the daemon starts and stops
  with the browser) over `--headless=new`; the User-Agent is rewritten with
  `Network.setUserAgentOverride` to the one the same build reports with a
  window (HTTP header *and* client hints); `Page.addScriptToEvaluateOnNewDocument`
  removes `navigator.webdriver` and the DevTools command line's
  `cdc_...`/`window.__proto__` names; the software GL renderer is answered as a
  desktop GPU. Measured on `receitasdepesos.com.br` (Chrome 145): headless
  returned the Cloudflare interstitial (`human_required`, 403), the same
  browser headful returned 200 and the real page. `cu diagnostics` gains a
  `stealth` block, and every page assessment reads the fingerprint values back
  and notes anything that still reads as automation. Notes are not repeated
  per navigation. The fast-test profile also drops
  `--remote-allow-origins=*`, which widened who may open the DevTools socket
  and was never needed (cu's websocket sends no `Origin`). Nothing here solves
  or evades a challenge -- a site that still wants a person gets the hand-off.

- Public login URL: `CU_PUBLIC_URL` / `cu start --public-url URL` tells the
  daemon the origin a reverse proxy serves `/login` under; `cu login` prints
  it. Requests are accepted for loopback `Host` names, plus the public host on
  `/login` only; any other `Host` gets 421. docs/README.md documents the
  Cloudflare + nginx (`location = /login`) + certbot setup.
- Screenshot control: `GET /v1/screenshot` takes `width`, `height` (viewport
  in CSS pixels), `scale` (device scale factor, e.g. `2` for retina-sharp),
  `ref` or `selector` (clip to one element) and `padding`; `cu shot` takes the
  same as `--width`, `--height`, `--scale`, `--ref`, `--selector`,
  `--padding`, plus `--png` and `--quality`. The override is cleared after
  the capture.
- Actions by snapshot ref: `POST /v1/click`, `POST /v1/type` and the batch
  `POST /v1/act` (`cu click`, `cu type`, `cu act`; `Client::click`,
  `Client::act`). A batch runs on one DevTools connection and ends with a
  snapshot. Refs stay with their element across snapshots and actions, and an
  old ref misses instead of hitting another element. Ten clicks in one batch
  take ~25 ms.
- Snapshots list the visible headings and name a `select` by its chosen option.
- `?context=NAME`: isolated browser contexts inside the one browser (~19 MB and
  ~35 ms each, against ~176 MB and 0.5-1 s for a second browser).
  `GET /v1/contexts`, `DELETE /v1/contexts/NAME`.
- `bench/compare.py`: cu against agent-browser and Playwright MCP on the same
  tasks, with snapshot token counts. Results in `docs/BENCHMARKS.md`.
- Tabs and popups: `GET /v1/tabs` (`cu tabs`) lists every tab with the default
  flagged, any page command takes `?tab=ID` (`--tab ID`) to work in another
  tab, and `DELETE /v1/tabs/ID` (`cu tabs close ID`) closes one (the last is
  refused). The default tab is pinned, so a popup and the browser's target
  ordering cannot move the page under the agent; a click that opens a window
  reports `"popup":true`.
- Downloads land in `<data>/downloads` instead of the user's real Downloads
  folder, through one persistent browser-level session (Chromium 145 honours
  a custom path only while such a session is attached). `GET /v1/downloads`
  (`cu downloads`) lists finished files, and the click that started a
  download reports `"download":"name"` with `navigated:false`. A download
  started from `?context=NAME` goes to `<data>/downloads/NAME`, because the
  browser-wide behaviour governs the default context only; `GET /v1/downloads`
  labels those entries with their `"context"`.
- Iframes are part of the snapshot and of the actions: every frame of the
  page is walked (subframes in their own isolated world), rendered under
  `- frame: <url>`, and a ref resolves to its frame, so click, type and
  select reach into embedded documents with the iframe's offset applied.
  `[contenteditable]` regions snapshot as textboxes.
- Open shadow roots are walked too, in the snapshot and in `/v1/text`. A web
  component renders its controls inside its shadow root, where a document
  query cannot see them, so a widget-heavy page used to snapshot as a page
  with nothing on it. Refs work there as well: the hit test descends through
  the host to the element really painted at that point. A closed shadow root
  is unreadable from any script and stays unread.
- `GET /v1/navigate` reports `settled:true|false`: false means the three
  second budget ran out on a page still loading.
- `GET /v1/text` (`cu text`, `Client::text`): the page's visible text for the
  reading a snapshot deliberately does not do -- prose, API responses,
  messages -- capped at 16 000 characters with a `truncated` flag. It reads
  every frame, each named under a `- frame: <url>` line, and reaches into open
  shadow roots, whose text no `innerText` alone returns.
- SDK: `tabs`, `close_tab`, `downloads`, `load_session`.

- Speed for agents, measured in `docs/BENCHMARKS.md`. Cold start went from
  431 ms to 6 ms (71x), and the new compact snapshot answers in about 2 ms.
- `GET /v1/snapshot` and `cu snapshot`: the page as one short line per
  interactive element, each with a stable `ref` an agent can act on, plus the
  headings. One CDP round trip, a few hundred bytes -- roughly a quarter of the
  size of the HTML, and far less than an accessibility tree. This is what an
  agent should read instead of a DOM dump or an image.
- Smart page settle on navigate. The daemon waits for the page to be
  interactive and to have stopped mutating, bounded to 3 s, and reports what it
  spent in `settled_ms`. Waiting for the network to go quiet hung for ever on a
  page holding an SSE stream or a websocket open; a navigate is now bounded.
- `cu start` returns before the browser is up. The daemon listens at once and
  Chromium starts on a thread that signals readiness, so start-up no longer
  blocks and the first page action waits only for the browser to really appear.
- JPEG screenshots by default (`optimizeForSpeed`, quality 60) with PNG still
  available on `GET /v1/screenshot?format=png`. `cu shot` names the file to
  match what came back.
- `bench/cu_bench.py`, which times start, navigate, snapshot and screenshot
  against a real Chromium over the loopback API.
- A "Parallel agents" section in the README and the landing: two rounds of
  real agent sessions (1 to 16 at once, MiniMax-M3.1-Flash-Preview driving
  `cu` against a production chat app, one `cu` context each inside the one
  shared Chrome) with the measured success rate, p50/p95, minimum available
  RAM, shared-Chrome and agent RSS, and 429 counts per level. The finding is
  that the bottleneck was the app's LLM provider rate limit and not RAM
  (available memory never fell below ~20 GB against a 3 GB floor): 8-10 agents
  are safe, 12 is the limit, 14+ is not recommended with that provider, and the
  marginal cost is ~0.3 GB per agent (~0.25 GB in the shared Chrome, ~0.06 GB
  for the agent process). Measured and extrapolated numbers are labelled apart;
  nothing above 16 agents was measured.

### Changed

- `navigated` in an action result means the frame committed a navigation
  (`Page.frameNavigated`); a download or an aborted load schedules and starts
  loading without committing and now reports `navigated:false`.
- A navigation Chromium cannot perform (unresolved name, refused connection)
  is a `502` with `navigation failed: net::...` instead of a `200` carrying
  `errorText` inside the CDP result.
- Navigate awaits `DOMContentLoaded` in the page in one round trip instead of
  polling `readyState` every 25 ms; navigate p95 went from ~59 ms to ~50 ms and
  median from ~45 ms to ~26 ms, interleaved against the previous binary.
- Chromium is launched with light flags (no background networking, updates,
  sync, extensions, crash upload, audio or GPU raster): ~30% less browser CPU
  on the same workload, one process fewer.

- DevTools connections are pooled and kept warm. Every action used to pay a
  `/json` discovery request, a TCP connect and a websocket handshake before it
  could send anything; a pool of eight connections now serves a burst without
  re-handshaking, and extras opened during a burst are dropped rather than
  cached. `TCP_NODELAY` is set on the CDP socket.
- The start-up poll for DevTools went from 50 ms to 2 ms, and the fixed 300 ms
  start-up sleep is gone: a browser that dies is reported on the first poll.

### Fixed

- `GET /v1/screenshot` works again on a browser that is not painting. The
  measure step that runs before the capture waited for two
  `requestAnimationFrame` callbacks with no bound, and a tab only gets those
  while the browser is producing frames for it: on a host with no GPU, a tab
  that is occluded or was never mapped by a window manager produces none, at
  all, ever. The `Runtime.evaluate` that awaited them never answered, the
  socket read timed out (5 s, twice over the retry) and every screenshot came
  back `{"error":"Resource temporarily unavailable (os error 11)"}` -- or, once
  the read happened to win the race against the stale frame, a 200 with
  `jpeg_base64: null` -- on *every* page and in both formats, while a raw
  `Page.captureScreenshot` on the same tab returned a full image in 30 ms. The
  frames are now raced against a 250 ms timer: a page that is painting still
  gets its two frames (about 33 ms), and one that is not is measured anyway and
  the existing frame is captured. A capture the browser refused is also
  reported as the error it is, instead of a 200 carrying a null image.

- The landing page no longer scrolls sideways. The screen-reader-only spans in
  the comparison tables were absolutely positioned with no positioned ancestor,
  so their `nowrap` text was laid out against the initial containing block and
  widened the document to a constant 1135 px at every viewport below that: at
  390 px the page could be dragged ~745 px to the right into empty space. The
  table wrapper is now `position: relative`, so each of those spans is anchored
  inside the table that owns it and clipped by that table's own horizontal
  scroller. The skip link is hidden with a clipped 1x1 box instead of
  `left: -999px`, which sat in negative overflow. The wide tables keep their
  own `overflow-x: auto` containers; no global `overflow-x: hidden`.
- The DevTools websocket handshake is read to its blank line; one `read()`
  could return only the status line under load and the action failed with
  "Chromium rejected CDP websocket handshake" (the load-sensitive flake in
  `a_client_that_stops_sending_is_released`).
- The browser readiness probe tries both loopback stacks (`127.0.0.1` and
  `[::1]`): with the IPv4 port held by a browser still shutting down,
  Chromium binds `::1` only and an IPv4-only probe never saw its own
  browser. The launched pid is recorded before readiness, so even a failed
  launch is closed on shutdown instead of orphaning a whole Chromium tree,
  and a slow start gets repeated watch windows instead of one final failure.
- `cu shot` shows the server's error instead of a generic "server did not
  return an image".
- Stopping `cu` closes its browser (`Browser.close`, then SIGTERM). Every
  daemon used to leave its whole Chromium tree running.
- CDP replies are matched on their top-level `id`; a nested frame `id` (as in
  `Page.getFrameTree`) could be taken for it.

- `launch_browser` hard-coded the DevTools port, so a daemon started with
  `CU_CDP_PORT` launched a browser nobody could reach and `cu navigate` failed
  with "connection refused". The configured port is used, and the daemon waits
  for the DevTools endpoint instead of racing browser start-up.
- Chromium is launched headless and with a single `--user-data-dir=PATH`
  argument. The browser used to die at once on a machine without a display, and
  read a separately-passed profile path as a second navigation target
  ("Multiple targets are not supported in headless mode", exit 13).
- A browser that exits immediately is reported instead of silently swallowed.
- DevTools HTTP responses are framed by `Content-Length` with a read timeout;
  reading to EOF blocked for ever against a browser that keeps the socket open.
- DevTools target discovery is parsed as JSON and filtered on `"type":"page"`,
  so navigate and screenshot no longer land on an extension background page or
  service worker that happens to be listed first.
- CDP websocket frames with a 64-bit length are read, which is what a real
  screenshot comes back in; they used to be rejected as "too large".
- `POST /login` types the credentials into the browser and submits the form.
  It used to write the username to a file and drop the password, so nobody was
  ever signed in. The password is still never stored, logged or returned.
- Form values are percent-decoded, so a password containing `+`, `%` or a
  non-ASCII character arrives intact.
- Profile symlinks are copied as symlinks and Chromium's `Singleton*` process
  locks are left out of a saved session. Following the links made `session
  save` fail with ENOENT, and copying a lock stopped the next browser from
  starting. Locks a killed browser left behind are cleared before launch.
- `cu session load NAME` now answers `{"loaded":true}` instead of
  `{"saved":true}`.

### Added

- `CU_HEADLESS=0` to run the browser against a display.
- Integration tests for login delivery, profile copies and session reporting,
  and unit tests for form decoding, JSON escaping and DevTools target
  selection.

- The server read a request with a single `read()` into a fixed buffer, so a
  client that wrote its request in several TCP segments was cut off
  mid-request and `cu status` failed intermittently. Requests are now read
  until the end of the header block and then up to `Content-Length`.

### Added

- Integration tests covering `status`, `navigate` and `screenshot`, plus a
  fake Chromium DevTools endpoint used by the tests.
- `CU_CDP_PORT` to point the daemon at a non-default DevTools port.

## [0.1.0] - 2026-10-06

- Added the local persistent Chromium session server and saved profile directory.
- Added bearer-authenticated status, navigation/session endpoints, and a secure login page.
- Added the `cu` CLI and Rust `Client` SDK.
- Added agent-facing documentation and security guidance.

