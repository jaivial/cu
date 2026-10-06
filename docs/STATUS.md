# STATUS

Work done on `feat/cu-speed` in this pass: the full `cargo test` suite is
green (70 tests), and cu was driven against real public sites -- form login
(the-internet.herokuapp.com, via both `cu act` and the `/login` form flow),
SSE and streaming pages (ntfy.sh SPA, an infinite `/.sse` stream that must
not hang navigate), heavy pages (bbc.com, react.dev), iframes (TinyMCE,
YouTube's hidden frames and consent dialog), popups
(the-internet.herokuapp.com/windows) and downloads
(the-internet.herokuapp.com/download).

Fixed along the way, each in its own commit: failed navigations and an
unsettled page are now reported as such; downloads land in the session's
directory and are reported by the click that started them; tabs and popups
are listable, addressable (`?tab=`) and closable, with the default tab
pinned; snapshots and actions reach into iframes; the websocket handshake
is read whole (a load-sensitive flake); the readiness probe tries both
loopback stacks, the launched pid is recorded before readiness, and a slow
browser start gets repeated watch windows instead of one permanent failure.

## Pending

- **`/v1/text` reads the main document only.** Text inside iframes is not
  included yet (the frame walk exists; merging per-frame text is straightforward).
- **Shadow DOM is not walked.** The snapshot traverses the light DOM only;
  interactive elements inside a web component's shadow root are invisible.
  Most sites keep controls in the light DOM (YouTube's consent buttons
  were), but widget-heavy pages may hide some.
- **Downloads in named contexts are unverified.** The browser-wide
  `Browser.setDownloadBehavior` is set without a `browserContextId`; where a
  download triggered from `?context=NAME` lands has not been checked. It
  should be given its own behaviour (and possibly listed separately).
- **Download completion is asynchronous.** The click result names the file
  when the download *starts*; `GET /v1/downloads` lists it when it is
  *finished*. A large file therefore needs a re-check a moment later. There
  is no progress endpoint (the events exist on the browser session and could
  be exposed later).
- **`press` watches the main frame.** `press` has no ref, so an Enter that
  submits a form *inside an iframe* reports `navigated:false` (the closing
  snapshot is still fresh). `click` and `type` follow the ref's frame.
- **`settled` exists only on `POST /v1/navigate`.** The navigate action
  inside `act` and the settle after a click report `navigated` but no
  `settled` flag.
- **Typing into the the-internet TinyMCE editor was not verified live.**
  The site leaves the editor `contenteditable=false` (readonly), so there
  was nothing to type into; the editable-iframe path is covered by the
  fake-driven integration test instead. A site with an editable embedded
  editor (most need accounts) would be a better live check.
- **Popup detection relies on `Page.windowOpen`** (emitted by Chromium
  145). An older browser may not emit it; `GET /v1/tabs` is the reliable
  check after a click that might have opened something.
- **`docs/BENCHMARKS.md` predates this pass.** Snapshots gained a
  `Page.getFrameTree` round trip (frame walking) and the daemon gained the
  browser control session; re-run `bench/compare.py` before quoting the old
  numbers.
- **No real-site tests in CI.** The live checks above were manual;
  `tests/fake_chromium.py` approximates them (including a child frame), but
  a recorded fixture or opt-in network test would catch regressions.
- **Load-sensitive test flakiness.** `a_client_that_stops_sending_is_
  released` failed twice under heavy load before the handshake-read fix.
  After it, one unidentified integration test failed once more in a run
  under parallel cargo/system load; its output was not captured, and the
  suite has been green for eight consecutive runs since (unit, integration
  and doc tests). If it recurs, save the whole `cargo test` output instead
  of re-running -- the status line of the failing assertion is what matters.
