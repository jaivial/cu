# Changelog

## [Unreleased]

### Added

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
  download reports `"download":"name"` with `navigated:false`.
- Iframes are part of the snapshot and of the actions: every frame of the
  page is walked (subframes in their own isolated world), rendered under
  `- frame: <url>`, and a ref resolves to its frame, so click, type and
  select reach into embedded documents with the iframe's offset applied.
  `[contenteditable]` regions snapshot as textboxes.
- `GET /v1/navigate` reports `settled:true|false`: false means the three
  second budget ran out on a page still loading.
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

