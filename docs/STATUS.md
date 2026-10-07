# STATUS

Work done on `feat/cu-speed` and `feat/cu-pending`: the full `cargo test`
suite is green (77 tests), and cu was driven against real public sites -- form login
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

Closed since, each in its own commit, on `feat/cu-pending`:

- **`/v1/text` reads every frame.** Each iframe is read in an isolated world
  of its own, like the snapshot does, and its words come back under a
  `- frame: <url>` line so they are never mistaken for the page's own. A
  frame that cannot be read is named on its line instead of failing the
  request: half the prose beats an error.
- **The snapshot walks open shadow roots.** `document.querySelectorAll` does
  not pierce a shadow root, so every control a web component rendered was
  invisible and a widget-heavy page snapshotted as a page with nothing on it.
  The walk takes the document and then each open root below it, and a host is
  skipped in its parent's pass because the root renders it rather than its
  children. Refs there are actionable: the hit test descends through the host,
  since `elementFromPoint` reports the *host* at the centre of a control
  inside it. A closed root exposes nothing to any script and stays unread.
- **A named context downloads into a directory of its own.** The
  browser-wide `Browser.setDownloadBehavior` governs the default context only,
  so `?context=NAME` downloads landed in the same directory as the
  persistent profile's own files. Each context now gets
  `<data>/downloads/NAME`, and `GET /v1/downloads` labels those entries with
  their `"context"`.

Each of the three was checked against real Chromium (headless, with a local
download server) before the tests were written, not only against the fake
browser. Two of them hid a bug the fake could not have shown: Chromium gives
a `ShadowRoot` no `innerText` at all, and `document.elementFromPoint` stops
at the shadow host.

## Pending

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
  browser control session, and both `/v1/text` and the snapshot now walk
  shadow roots on top of that. Re-run `bench/compare.py` before quoting the
  old numbers.
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

- **Shadow roots were checked against a local fixture, not a widget-heavy
  public site.** The walk itself is verified against real Chromium (open,
  nested, slotted and closed roots, `display:none` content, and that the
  light-DOM output is unchanged), but no real site known for custom elements
  was driven end to end. The live checks in this file remain the manual part
  of the suite; `bench/compare.py` and the fixtures under `tests/` are what
  the CI would run.
