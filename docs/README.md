# cu

`cu` keeps one Chromium profile alive for an agent instead of launching a new browser per action. The profile is stored below `.cu/profiles/default`, and named copies are stored below `.cu/sessions`.

## Quick start

```sh
cargo run -- start
cargo run -- status
cargo run -- navigate https://example.com
cargo run -- login
cargo run -- session save example
```

The HTTP API is loopback-only and bearer-token protected. `GET /login` and `POST /login` are intentionally unauthenticated so a human can complete a login without exposing a password to an agent. The form response never includes the password.

`session save` copies the live profile to `.cu/sessions/NAME` and `session load NAME` copies it back, so a logged-in session survives a restart. Chromium's per-process lock files are not part of a session and are left out of the copy.

### Snapshot and screenshot

`GET /v1/snapshot` (or `cu snapshot`) returns the page as an LLM wants it: one
line per interactive element, each with a stable `ref` the next action can name,
plus the headings. It is a couple of CDP round trips and a few hundred bytes --
use it instead of reading the DOM or OCRing an image.

```json
{"snapshot":"- url: https://example.test/\n- page: Example\n- ref=e1 link \"More\"\n"}
```

If the page embeds other documents, each iframe is walked too and its
elements follow a `- frame: <url>` line, with refs from the same `eN`
space (one band per frame, so two frames never hand out the same ref).
An editable region (`contenteditable`, as in a rich-text editor) appears
as a `textbox`. A frame that cannot be read says so on its line rather
than failing the snapshot.

Controls inside a web component's *open* shadow root are walked too, and
carry refs like any other: a page built from custom elements would
otherwise snapshot as a page with nothing on it. A closed shadow root is
invisible to every script and stays unread.

A snapshot is deliberately not a document dump: it shows what an agent
can act on and where it is. When the answer itself is words -- prose, an
API response, a message -- `GET /v1/text` (`cu text`, `Client::text`)
returns the page's visible text instead, capped at 16 000 characters
with `"truncated":true` when the page was longer. It reads every frame,
each named under its own `- frame: <url>` line, and reaches into open
shadow roots.

Screenshots are JPEG by default because a model reads a lossy frame just as
well and the capture, encode and transfer all shrink. `GET
/v1/screenshot?format=png` gives a lossless PNG back; `cu shot FILE` writes
either, naming the file to match (a FILE ending in `.png`, or `--png`, asks for
PNG).

When the pixels are for people (docs, slides, social posts) the capture can be
shaped:

| query | `cu shot` flag | what it does |
|---|---|---|
| `width=N`, `height=N` | `--width N`, `--height N` | viewport in CSS pixels for this capture (1-8192); a side left out keeps the browser's |
| `scale=F` | `--scale F` | device scale factor, above 0 and up to 4: `2` is a retina-sharp image with twice the pixels per side |
| `ref=eN` | `--ref eN` | clip to the element behind a snapshot ref |
| `selector=CSS` | `--selector CSS` | clip to the first element matching a CSS selector |
| `padding=N` | `--padding N` | CSS pixels kept around a clipped element |
| `quality=N` | `--quality N` | JPEG quality, 1-100 (default 60) |

```bash
cu shot page.png --width 1200 --height 1200 --scale 2       # 2400x2400 PNG
cu shot card.png --width 560 --scale 2 --selector '#pricing .card' --padding 12
cu shot button.jpg --ref e7 --scale 3
```

The viewport and scale go through `Emulation.setDeviceMetricsOverride` for
that one capture and are cleared afterwards, so later actions and snapshots
see the page as before. The page gets two animation frames to lay out at the
new size before it is measured. A clipped element is scrolled into view and
captured with `Page.captureScreenshot`'s `clip`; one larger than the viewport
is taken from the full page. With `scale` the reply also carries the image
size: `{"format":"png","width":2400,"height":2400,"scale":2,"png_base64":...}`.
`ref` and `selector` together, an out-of-range value or an element that is not
on the page (or has no size) are a `400`/`502` with the reason.

Navigation waits for the page to be interactive and stable before it answers,
bounded to three seconds, and reports what it spent in `settled_ms`. A page
that keeps a stream open for ever -- SSE, websocket, long poll -- therefore
costs at most three seconds instead of hanging the action. The reply carries
`settled:true|false`: `false` means the budget ran out on a page still
loading, so take a snapshot before acting. A navigation the browser cannot
perform (a name that does not resolve, a refused connection) is a `502` that
says `navigation failed: net::...`, not a quiet success.

### Acting on refs

`POST /v1/click {"ref":"e3"}` and `POST /v1/type {"ref":"e2","text":"hi","submit":true}`
act on an element from the last snapshot (`cu click e3`, `cu type e2 hi --submit`).
`POST /v1/act` runs a batch on one connection and ends with a fresh snapshot,
so a whole form is one call:

```json
{"actions":[
  {"do":"type","ref":"e2","text":"Ada"},
  {"do":"select","ref":"e4","value":"Medium"},
  {"do":"click","ref":"e5"}
]}
```

Actions are `click`, `type` (`text`, `clear` default true, `submit`), `press`
(`key`: Enter, Tab, Escape, arrows, ...), `select` (`value` or label),
`navigate` (`url`) and `wait` (`ms`). `"snapshot":false` skips the closing
snapshot. The reply reports `ok`, per-action `navigated` and `ms`, and on a
failure the index that `failed` and why -- a stale ref says to take a new
snapshot, a covered element names what covers it. Refs stay with their element
across snapshots and actions.

`navigated` means the frame really committed a navigation. A click whose
result starts a download instead of changing the page reports
`"download":"file.name"` with `navigated:false`, and a click that opens a
window or tab reports `"popup":true` -- run `GET /v1/tabs` to see it.

### Contexts

Add `?context=NAME` to navigate, snapshot, screenshot or act to work in an
isolated browser context of the same browser (own cookies and storage),
created on first use. `GET /v1/contexts` lists them and `DELETE
/v1/contexts/NAME` closes one. Stopping `cu` closes its browser.

### Tabs and popups

The one default tab is pinned: popups and the browser's own target order
cannot move the page under the agent. `GET /v1/tabs` (`cu tabs`) lists
every tab with its `id`, `url` and `title`, flagging the default.

Any page command takes `?tab=ID` (`cu navigate URL --tab ID`,
`cu snapshot --tab ID`, ...) to work in another tab -- the popup a click
just opened, for instance. `DELETE /v1/tabs/ID` (`cu tabs close ID`,
`Client::close_tab`) closes one; the last tab is refused, because
Chromium exits with it. Giving both `?context=` and `?tab=` is an error.

### Downloads

Downloads land in `<data>/downloads`, not in the user's real Downloads
folder, so files an agent fetched are where it can see them.
`GET /v1/downloads` (`cu downloads`, `Client::downloads`) lists the files
that have finished, newest first; a file still being written is not
listed yet, so check again after a moment. The click that started the
download already named it in its result. A download started from
`?context=NAME` goes to `<data>/downloads/NAME` and is listed with its
`"context"`, because it is a different file from the default tab's.

Latency for every one of these is tracked in [BENCHMARKS.md](BENCHMARKS.md),
including a comparison with agent-browser and Playwright MCP.

The browser executable defaults to `chromium`; set `CU_BROWSER` to an alternate binary. Browsers are launched headless, which is what an agent usually wants; set `CU_HEADLESS=0` to attach a display instead. `CU_DATA_DIR` changes the default data directory and `CU_CDP_PORT` the DevTools port (default `9222`). Do not expose this server beyond localhost without adding TLS and an access-control layer.

### Browser mode: fast-test and compatibility

`cu start` launches the browser in one of two profiles, chosen with `CU_MODE=fast|compat` or `cu start --mode fast|compat` (`--compat` for short):

- **fast-test** (default): the light profile cu has always used. GPU, extensions, site isolation, background networking and a list of features are switched off to save start-up time, processes and memory. Meant for sites you own and test.
- **compatibility**: a stock full Google Chrome or Chromium (Chrome is preferred when `CU_BROWSER` is unset) with only `--no-first-run`, `--no-default-browser-check` and `--password-store=basic`. Nothing is disabled and nothing is spoofed. A headless-shell build (`chrome-headless-shell`, Playwright's `headless_shell`) is refused in this mode, because it is a separate, older browser that no flag turns into the full one; headless runs use the full browser's `--headless=new`. For the most normal setup run headful (`CU_HEADLESS=0`) on a real display.

The daemon logs the executable it spawned (and where the symlink points), the product version, the headless mode and the GL renderer, plus plain notes such as "the User-Agent says HeadlessChrome" or "software rendering". The same record is written to `DATA/browser.json` and served, with DevTools protocol counters (commands, errors, latency, main-world vs isolated evaluations, `Runtime.enable` calls -- which should stay 0), at `GET /v1/diagnostics` (`cu diagnostics`). These are observations about the setup; neither mode guarantees how a site will treat the browser.

## Login

`POST /login` takes `username` and `password` form fields. The daemon types them into the page the browser is showing and submits the form, which is how the credentials reach the site without ever passing through an agent. The password is not written to disk, not logged and not included in the response; the reply only says whether it could be typed. Navigate to the sign-in page first, or the submission has nowhere to go.

## SDK

```rust
let cu = cu::Client::new("127.0.0.1:8787", token);
println!("{}", cu.status()?);
```

Besides `status`, `navigate`, `snapshot`, `click` and `act`, the client
has `tabs` and `close_tab` for popups, `downloads` for finished files,
and `save_session`/`load_session` for profiles.
