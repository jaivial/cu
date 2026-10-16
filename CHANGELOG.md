# Changelog

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

