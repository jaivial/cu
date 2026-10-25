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

`session save` writes the live profile to `.cu/sessions/NAME.cuse`, encrypted, and `session load NAME` puts it back, so a logged-in session survives a restart (see *Sessions and identity* below). Chromium's per-process lock files and caches are not part of a session and are left out.

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

### Visual QA references

A snapshot names elements and a screenshot shows pixels; these routes tie the
two together and to the code, so a QA finding says *which* element and *where
it comes from* instead of "the blue button near the bottom".

| route | CLI | what it returns |
|---|---|---|
| `GET /v1/inspect?ref=eN` / `?selector=CSS` | `cu inspect eN`, `cu inspect --selector CSS` | the element's ref, role, accessible name, a unique selector, test id, landmarks around it, box, viewport, key computed styles and state; on a development build also `framework`, `component`, `components` (the owner chain) and `source` |
| `POST /v1/marks` `{"ref":"eN"\|"selector":"CSS","note":"...","severity":"major"}` | `cu mark eN "note" [--severity S]` | a QA mark: the note plus the inspect object at that moment |
| `GET /v1/marks` / `?format=md` | `cu marks`, `cu marks --md` | every mark of the tab, as JSON or as a Markdown report |
| `DELETE /v1/marks` | `cu marks clear` | forget the tab's marks |
| `GET /v1/screenshot?annotate=e1,e4` / `annotate=all` | `cu shot --annotate e1,e4\|all` | the image with those snapshot refs boxed and labelled |
| `GET /v1/screenshot?marks=1` | `cu shot --marks` | the image with the marks on this URL boxed, numbered like the report and coloured by severity |

- **Selectors** prefer an id, then `data-testid`/`data-test`/`data-cy`/`data-qa`,
  then `name`, and fall back to a short `nth-of-type` path, checked unique in
  its root. A control inside an open shadow root gets `host >>> inner`; the
  `selector` parameters accept that form too.
- **Components and sources** are read in the page's own world (where React's
  fibers, Vue's `__vueParentComponent`, Svelte's `__svelte_meta` and Angular's
  `ng` live) by reading properties only -- no page function is called. React
  ≤18 gives `_debugSource` (file:line:col); React 19 gives the served module
  from `_debugStack`. Build plugins that write attributes are read too
  (`data-inspector-*`, `data-v-inspector`, `data-astro-source-*`,
  `data-source-file`/`-line`, `data-component`). Production builds usually
  give nothing here, and the fields are then left out.
- **Overlays** are drawn in a closed shadow root at the top of the document
  (document coordinates, `pointer-events:none`), in the frame each ref lives
  in, for that one capture, and removed right after it. The reply lists what
  was drawn in `"annotated"`.
- **Privacy**: inspect never reads a field's value, so a mark on a password or
  email field carries no secret. Marks live in daemon memory per tab.
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
`navigate` (`url`), `wait` (`ms`) and `upload` (`files`: paths; `ref`: the
file input or the button that opens the picker -- file inputs are usually
`display:none` and never get a ref of their own, so cu finds the input at,
labelled by, inside or around that control; no `ref` = the page's only file
input). `upload` uses `DOM.setFileInputFiles`, fires `input`/`change`, refuses
several files on an input without `multiple`, and fails (leaving the input
empty) when the browser cannot read a file -- snap Chromium has a private
`/tmp`, keep uploads under `$HOME`. CLI: `cu upload [REF] FILE...`. `"snapshot":false` skips the closing
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

A context lives in memory only. `cu session save NAME --context CTX`
(`POST /v1/session/NAME?context=CTX`) saves its cookie jar
(`Storage.getCookies` for its `browserContextId`) to
`sessions/NAME.ctx.cuse`, encrypted with the same key and format as profile
sessions; `cu session load NAME --context CTX` (`.../NAME/load?context=CTX`)
creates the context if needed and puts the cookies back into it live
(`Storage.setCookies` with its `browserContextId`), in this or a later
daemon. Context archives authenticate under their own name space, so one
cannot be loaded as a profile session (or renamed) without failing. Only
cookies: a context's localStorage/IndexedDB are not saved.

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

`CU_TRACE=1` makes the daemon print one line per step it takes
(`TRACE cdp Runtime.evaluate 0.34`, `TRACE navigate.after_settle 7.34`, ...)
on its stderr: where the time of an action went, down to the DevTools round
trips. It is off by default and changes nothing else.

The browser executable defaults to `chromium`; set `CU_BROWSER` to an alternate binary. Browsers are launched headless, which is what an agent usually wants; set `CU_HEADLESS=0` to attach a display instead. `CU_DATA_DIR` changes the default data directory and `CU_CDP_PORT` the DevTools port (default `9222`). Do not expose this server beyond localhost without adding TLS and an access-control layer; the one supported exposure is the login form behind a reverse proxy (see *Public login URL*).

### Browser mode: fast-test and compatibility

`cu start` launches the browser in one of two profiles, chosen with `CU_MODE=fast|compat` or `cu start --mode fast|compat` (`--compat` for short):

- **fast-test** (default): the light profile cu has always used. GPU, extensions, site isolation, background networking and a list of features are switched off to save start-up time, processes and memory. Meant for sites you own and test.
- **compatibility**: a stock full Google Chrome or Chromium (Chrome is preferred when `CU_BROWSER` is unset) with only `--no-first-run`, `--no-default-browser-check` and `--password-store=basic`. Nothing is disabled and nothing is spoofed. A headless-shell build (`chrome-headless-shell`, Playwright's `headless_shell`) is refused in this mode, because it is a separate, older browser that no flag turns into the full one; headless runs use the full browser's `--headless=new`. The launch defaults to headful on an X display (see below); `CU_HEADLESS=1` asks for `--headless=new` back.

The daemon logs the executable it spawned (and where the symlink points), the product version, the headless mode and the GL renderer, plus plain notes such as "the User-Agent says HeadlessChrome" or "software rendering". The same record is written to `DATA/browser.json` and served, with DevTools protocol counters (commands, errors, latency, main-world vs isolated evaluations, `Runtime.enable` calls -- which should stay 0), at `GET /v1/diagnostics` (`cu diagnostics`). These are observations about the setup; neither mode guarantees how a site will treat the browser.

### The browser looks like a person's

A headless Chrome says so three times: `HeadlessChrome` in the User-Agent, the same token in the client hints a page reads as `navigator.userAgentData`, and the launch flags themselves, readable from `/proc/<pid>/cmdline` by any page the site drops in a frame it controls. Cloudflare's interstitial scores exactly that: on `receitasdepesos.com.br` (Chrome 145, measured October 2026) the headless launch answered the navigation with `human_required` -- "Just a moment...", 403, `cf-mitigated: challenge` -- and the same browser with a window on an X display answered 200 with the real page.

So the default launch has changed, and a hardening pass runs once the browser is up:

- **A window instead of `--headless`.** `cu start` looks for a display (`CU_DISPLAY`, then an existing `DISPLAY`, then the first free display number for an `Xvfb` it starts itself at the profile's window size) and launches the browser there; the X server it started is stopped with the browser. No display, or `CU_HEADLESS=1`, is the only way back to `--headless=new`, and the fallback is logged. The launch record keeps saying which it was.
- **A windowed User-Agent.** One coherent string, the product token taken from the browser's own `/json/version` so `Chrome/145` is the build that really answers, installed with `Network.setUserAgentOverride` -- which rewrites the HTTP header *and* the client hints (a `--user-agent=` flag would leave `navigator.userAgentData` still saying HeadlessChrome).
- **No automation surface.** `Page.addScriptToEvaluateOnNewDocument` (page world, `runImmediately`, `includeCommandLineAPI`) removes `navigator.webdriver` and restores the `window.__proto__` / `cdc_adoQpoasnfa76pfcZLmcfl_*` names that the DevTools command line leaves behind -- the first thing every automation library checks, and the thing a real browser drops on the floor.
- **No software renderer on display.** A host with no GPU answers `WEBGL_debug_renderer_info` with `SwiftShader`, which is the browser's one unambiguous "there is no machine here"; cu answers with what a desktop GPU reports instead, and says so in `cu diagnostics` (`stealth.notes` names the value it reported). It is the only value cu invents, and everything else -- locale, time zone, window, platform, CPU count -- stays this host's, on purpose.

Off with `CU_STEALTH=0`. `cu diagnostics` has a `stealth` block with what was applied, and every page assessment reads the values back from the page's own world and notes any that still read as automation. Nothing here solves a challenge, clicks a checkbox or forges a clearance cookie: a site that still wants a person gets the hand-off below. A bot score, an IP reputation and a fingerprint database are what a defence looks at beyond the browser.

### Sessions and identity

- **One identity per profile, from this host.** The first launch on a profile derives its locale (`LC_ALL`/`LC_MESSAGES`/`LANG`), time zone (`TZ`, `/etc/timezone`, `/etc/localtime`) and a common desktop window size, and stores them in the profile (`cu-identity.json`, along with a seed the paced input uses). Every later launch applies the same values at the process level -- `--lang`/`--accept-lang`, the browser's own `TZ`, and in headless `--window-size` plus a matching `--screen-info` -- so pages, iframes and workers agree. The user agent and platform are the browser's own: cu never claims an OS the host does not run. `CU_LOCALE`, `CU_TIMEZONE`, `CU_WINDOW_SIZE` (`1440x900`) override a value on purpose; the override is stored, and reported as such, because it must match the network the browser uses. Once the browser is up cu reads back what a page sees (language, `Intl` zone, platform, screen, viewport) from its isolated world and notes any mismatch; all of it is under `session` in `cu diagnostics`.
- **One owner per profile.** The daemon holds an exclusive lock on `DATA/owner.lock` for its life; a second `cu start` on the same data directory exits with the owner's pid and ports instead of clearing the first one's browser locks.
- **Encrypted saved sessions.** `cu session save NAME` writes `sessions/NAME.cuse`: each profile file is an XChaCha20-Poly1305 record (RustCrypto `chacha20poly1305`) bound to the session name and its position, ending with a record count, so a modified, renamed, reordered or truncated archive is refused rather than half-loaded. The browser's live cookie jar is saved too (Chromium writes cookies to disk only every ~30 s) and put back into the browser on load. The key is `CU_SESSION_KEY` (64 hex) or the key file `CU_SESSION_KEY_FILE` (default `~/.config/cu/session.key`, created 0600 on first save and refused if group/world readable) -- outside the data directory, so a copied `.cu` does not carry its key. A load decrypts into a staging directory and touches the profile only when every record verified. Sessions saved before encryption (plain directories) still load; `CU_SESSION_PLAINTEXT=1` keeps the old plain copy. As before, load into a stopped browser (or restart after loading) for local storage to come back; cookies are restored either way.

### Challenge state

After every navigation (`cu navigate`, and any batch action that navigated) cu reads the page from its isolated world and reports a `challenge` object: `state` is one of `ready`, `challenge_pending`, `human_required`, `blocked`, `rate_limited` or `unknown`, plus the `vendor` it recognised (Cloudflare interstitial/Turnstile/block pages, DataDome, PerimeterX, Imperva, Akamai, AWS WAF, reCAPTCHA, hCaptcha, Arkose), the HTTP `status`, the `signals` it rests on and a `next` hint. Only a 2xx/3xx http(s) page with no signal is `ready`; a bare 403/503, a page still loading or a probe that failed is `unknown`, never success. URLs are cut to origin and path (challenge URLs carry tokens), and no cookie or token is read.

- An interstitial ("Just a moment...") is given `CU_CHALLENGE_WAIT_MS` (default 10000, max 60000) to clear by itself; one that does not becomes `human_required`.
- `human_required` pauses automation on that tab: `navigate`, `act`, `click` and `type` answer `409` until a re-check shows the challenge gone (a person solved it, e.g. in the windowed browser cu launches by default), or `cu challenge release` hands the tab back. Reads (`snapshot`, `text`, `shot`) stay allowed.
- A batch stops after a navigation that lands on `human_required`, `blocked` or `rate_limited`, and names the action that was not run.
- Hand-off: when a tab turns `human_required`, a headful browser (`CU_HEADLESS=0`) brings that tab to the front (`Page.bringToFront` + `Target.activateTarget`); the hand-off is logged and, with `CU_HANDOFF_NOTIFY` set, that command runs via `sh -c` with `CU_HANDOFF_EVENT` (`begin`/`cleared`/`released`/...), `CU_HANDOFF_TAB`, `CU_HANDOFF_URL` (origin+path) and `CU_HANDOFF_VENDOR` in its environment (e.g. `notify-send "cu: $CU_HANDOFF_EVENT" "$CU_HANDOFF_URL"`). A watcher re-checks the tab every second like the gate, so it is released as soon as the page reads ready, without waiting for the agent's next action (for `CU_HANDOFF_WATCH_S`, default 900 s; the gate keeps re-checking after that). Each pass is kept in `cu diagnostics` under `challenge.handoffs` (last 20): vendor, signals, outcome, final state/URL, duration, re-checks, whether the tab was brought forward, and the cookies that appeared meanwhile -- `name@domain` only, never values.
- `cu challenge` (`GET /v1/challenge`) re-checks the current page on demand. `CU_CHALLENGE=0` turns the post-navigation check off for owned test sites.

### Waiting out a hand-off

A `human_required` navigation used to leave the agent with a bad choice: retry and read the same `human_required` forever, or give up. Both are wrong -- a blind retry burns the session's reputation and a blind give-up throws away a challenge that resolves on its own. So the hand-off is now observable and awaitable, and `cu` resumes the navigation the moment the verification resolves, with no retry by the agent.

- **`GET /v1/handoff`** (`cu handoff`) reports the hand-off state of the tab without probing the page: `waiting` (bool), `open` (the hand-off itself when there is one -- tab, redacted URL, vendor, `open_ms`, re-checks, whether the tab was brought forward, the signals) , the last `assessment`, the wait `budget` the daemon will clamp to, and the `history` of passes. `waiting:false` with no open hand-off is the answer "no person is needed"; `waiting:true` with a growing `open_ms` is "a person still is".

- **`GET /v1/challenge?wait=S`** -- equivalently `cu challenge wait --timeout S` or `cu handoff wait` -- parks until the hand-off resolves instead of answering at once. It re-checks with the same gate the background watcher uses (never interacting with the tab) and returns the assessment it ended on plus `resolved`, `timed_out`, `waited_ms` and `polls`. It returns as soon as the page reads `ready` (the person finished), or when the verdict turns into something no person can fix from here (`blocked` / `rate_limited`), so it never burns its whole budget on a hopeless page. `timed_out:true` means the person has *not* finished and the tab is still paused; the CLI exits non-zero in that case, so a script or an agent can branch on it without parsing JSON.

  The real case this was measured against: `https://help.x.com/es/forms/account-access/regain-access` (the Spanish account-recovery form) answers a `human_required` / `cloudflare` verdict -- a Cloudflare managed challenge on an ordinary GET. `cu handoff wait --timeout 120` parks on it; when the challenge clears, the wait returns `resolved` with `ready` and the agent simply continues from the navigation that was already in flight, rather than issuing another `navigate` into a page that would only re-trigger the defence.

- **Bounded by construction.** Whatever `wait` asks for is clamped to `CU_HANDOFF_WAIT_MAX_S` (default 900 s); the re-checks use a fixed backoff -- 250 ms, 500 ms, 1 s, 2 s, 4 s and then every 4 s -- and the probe count is capped (`budget / 4 s`, at most 240). So one long wait is one parked daemon thread, not one request per second, and it cannot spin; asking for an hour against a 2 s ceiling returns at the ceiling with a bounded probe count. The challenge URL's `__cf_chl_tk` is cut to origin and path everywhere, including in the wait reply, so nothing about the token ever reaches the agent.

`cu` still does not solve, click through or evade a challenge: it reports one, hands the window to a person, waits with a bounded budget, and resumes when it resolves.

Vendor signals tuned against real challenge pages (captured once from public sites and replayed from an owned mock): Cloudflare's `_cf_chl_opt` config and `#challenge-error-text` (language-independent: the title is localised) and the `cf-mitigated: challenge` response header; DataDome's inline `dd` config before its frame exists -- `t:'bv'` block, `rt:'c'` CAPTCHA, `rt:'i'` device check (treated as an interstitial) -- and `x-datadome: protected` on an otherwise unmarked 4xx. The main document's status is now taken from its own response (the navigation's loader id), not the first document response seen.

Batches observe the network around each `navigate` action too (`Network` on only for that navigation, bodies never buffered), so a batch navigation reports `status`, honours `Retry-After` (`origin_closed_ms`) and feeds the header hints to the probe, like `cu navigate`.

`cu diagnostics` has a `challenge` block: assessments by state and vendor, `challenge_rate` (defence verdicts / assessments), `false_success_rate` (ready verdicts whose next check on the same tab and origin found a defence / ready verdicts -- a lower bound: only what a later check saw), and hand-offs cleared vs released. The gate's re-checks of a paused tab are not counted as new assessments.

First-party account checkpoints (vendor `first-party`): a site's own "suspicious login" / "confirm it's you" / code-by-SMS-or-email / 2FA page has no vendor marker and answers 200, so it used to read as `ready`. Three signals now: a checkpoint path (`/challenge/`, `/checkpoint/`, `/auth_platform/`, `/two_factor`, `/2fa`, `/mfa/`, ... also in a `#/` hash route), a visible one-time-code form (`autocomplete=one-time-code`, a field named/labelled security/verification/confirmation/login code, OTP, or a row of 4-8 single-character boxes; card CVV fields excluded) and the wording (English and Spanish). Path + any form field, path + wording, or code form + wording is `human_required`; one signal alone is `unknown` (never `ready`, not gated).

Chromium's own renderer refusal page (vendor `chrome-deny`, state `blocked`): sites that refuse the client outright -- X/Twitter and several Cloudflare-fronted origins do it today -- do not even let the origin's own HTML through; Chromium answers the navigation with `chrome-error://chromewebdata/` ("Access to <site> was denied", "You don't have authorization to view this page. HTTP ERROR 403"). That page is not an http(s) page, so it used to read as ready via the `not an http(s) page` branch. It is classified before that branch: it is blocked, the vendor names it `chrome-deny`, and the word the classification hangs on ("access to", "you don't have authorization", "http error 403/401/429") and the site's name (the error page's title) are the signals. The `next` hint on a `chrome-deny` vendor is its own; a plain 403 body from the origin itself without a known vendor marker is still `unknown`.

The refusal reaches `cu navigate` along two paths, both reported the same way. When Chromium still renders a page, the reply carries the `challenge` object described above. When the origin's status is one Chromium will not render at all -- X/Twitter's 403 to an automated client -- `Page.navigate` answers `net::ERR_HTTP_RESPONSE_CODE_FAILURE` and the tab is left on the same error page: the navigation is then assessed before the failure is reported, and the reply is `200` with `navigated:false`, the browser's `error`, the `challenge` object and the `status` the network saw (`crate::scheduler::http_level_failure` holds the list of those errors, and they are never auto-retried). A transport failure -- a name that does not resolve, a refused connection -- answers `502` as before: nothing answered, so there is no page to classify.

Single-page apps: the daemon's browser-level connection already sees `Target.targetInfoChanged` for every page; when a tab cu has assessed changes address without a cu navigation (`history.pushState`/`replaceState`, a hash change, a redirect the page made itself), it is re-checked 400 ms later (the view renders after the URL) and the state on record -- and so the gate -- follows: a feed that turns into a checkpoint becomes `human_required` (and counts as a contradicted `ready`), a checkpoint the person completed that routes back into the app releases the hand-off. A re-check that sees the same URL and state changes nothing. Counted in diagnostics as `url_rechecks` / `url_recheck_changes`.

cu does not solve, click through or evade challenges; it reports them and stops.

### Helper isolation

Every helper cu runs in a page (snapshot walk, ref registry, text read, settle wait, screenshot measure, challenge probe) evaluates in cu's own isolated world of the frame: a separate JavaScript global over the same DOM. The page's scripts do not see `window.__cu`, cannot observe the evaluations and cannot shadow the built-ins the helpers use; cu never sends `Runtime.enable`. The page's main world is used only for the batch's no-op barrier. `CU_HELPER_WORLD=main` puts the main-document helpers back in the page's world, if a page ever needs it.

### Out-of-process iframes, popups and workers

With site isolation on (the compatibility profile) a cross-site iframe is rendered by another process: it is missing from the page's frame tree and used to vanish from snapshots and text without a word. cu now finds those out-of-process iframes as DevTools targets (`type: iframe`, nested ones by `parentId`), walks each in cu's isolated world of its own process -- the same helpers, the same ref bands, the page still never sees `__cu` -- and acts on their refs through that target (focus, type, click; the click point is translated through every owning iframe element, whichever process holds it), in instant and paced input alike. In fast-test, where those frames share the page's process, nothing changes.

The daemon's browser-level connection also turns on target discovery (observation only; nothing is attached to pages or workers): `cu targets` (`GET /v1/targets`) lists live pages, iframe targets and dedicated/shared/service workers with their origin+path, opener and context, plus lifetime totals; `cu tabs` names the tab that opened each popup (`opener`), and `cu diagnostics` carries the live counts. A browser-level `Target.setAutoAttach` was tried and only attaches top-level pages; reaching frames and workers that way would mean holding a session on every page for its whole life.

### Per-origin budgets

Every navigation cu starts (`cu navigate`, `navigate` actions in a batch) takes a slot on its origin first:

| | fast-test | compatibility | override |
|---|---|---|---|
| navigations in flight per origin | 16 | 2 | `CU_ORIGIN_CONCURRENCY` |
| minimum gap between starts | 0 | 1000 ms | `CU_ORIGIN_INTERVAL_MS` |
| automatic retries (429/503, transient network errors) | 0 | 2 | `CU_ORIGIN_RETRIES` (max 5) |
| longest wait for a slot before failing | 30 s | 30 s | `CU_ORIGIN_MAX_WAIT_MS` |

A 429/503 with `Retry-After` closes the origin until then (capped at one hour). A challenge, block or rate-limit page without the header backs the origin off exponentially (2 s, 4 s, ... up to 5 min), and a ready page resets it. Challenges and blocks are never retried automatically. A navigation that would have to wait longer than the max-wait fails at once with `429` and `retry_in_ms`, so the agent is never left blocking. The navigate reply adds `status`, `retries`, `queued_ms` and `origin_closed_ms` when they apply, and `GET /v1/diagnostics` lists every origin's state. Only the main document's status and `Retry-After` are read from the response; no other header and no cookie.

### Input pacing

`CU_INPUT=paced` makes clicks, scrolling and typing look like real hardware input, structurally. The model is Jaime's `human.rs` (from `feat/human-input`, ported from `invisible_playwright`'s behaviour layer):

- **one hand per session**: curvature, speed, tremor, overshoot, typing rhythm and click dwell are drawn from a seed -- the profile identity's seed (see *Sessions and identity*), mixed with the context name for `?context=` -- so a profile keeps its hand across restarts and two profiles differ. Every movement is still a fresh draw; `CU_SEED=N` replays a run exactly;
- the pointer moves along a Bezier path walked by arc length with a bell-shaped speed profile, a Fitts-law duration, zero-mean tremor and an occasional overshoot pulled back, sampled no faster than a real mouse (8 ms); a fresh tab's pointer starts somewhere plausible (never 0,0); it lands inside the middle of the target box, and the button is held for a log-normal dwell;
- a target off screen is scrolled to with the wheel (`mouseWheel` notches of 100 px in flicks), not teleported; inside an inner scroller (a feed, a modal, a sidebar) the pointer first moves over that scroller's visible box and the wheel turns there, so the scroller -- not the document -- moves; a scroller that ignores the wheel, or an iframe, falls back to the instant scroll;
- a field is reached with **Tab** when it is the next one after the focused element, or **Shift+Tab** when it is the previous one, in the browser's sequential focus order (positive `tabindex` first, ascending, then document order; inert and hidden controls skipped; radio groups not modelled, they get a click), otherwise with a click; old text is removed with Ctrl+A and Backspace; every character is a `keyDown`/`keyUp` pair with a per-key hold and a gap that is shorter across hands, longer on one hand or a repeated key, with occasional hesitations.

Nothing is ever mistyped: the field receives exactly the given text, which matters for credentials. A 12-character field takes a few seconds instead of ~2 ms, so the default stays `instant` (one `Input.insertText`, press/release on the spot) for owned sites and CI. Plausible timing is not evidence of a person; it only removes the structural differences between cu's input and a real user's.

## Login

`POST /login` takes `username` and `password` form fields. The daemon types them into the page the browser is showing and submits the form, which is how the credentials reach the site without ever passing through an agent. The password is not written to disk, not logged and not included in the response; the reply only says whether it could be typed. Navigate to the sign-in page first, or the submission has nowhere to go.

## Public login URL

The daemon always binds to `127.0.0.1`. To let a person open the login form from another machine (a phone, a laptop) without exposing the API, put a TLS reverse proxy in front that forwards **only** `/login`, and tell the daemon its public URL:

```sh
CU_PUBLIC_URL=https://login.example.com cu start --port 8801
# or: cu start --port 8801 --public-url https://login.example.com
cu login   # -> Open https://login.example.com/login in a browser. ...
```

- `CU_PUBLIC_URL` / `--public-url` takes a bare origin (`https://host[:port]`, no path). It is checked at start, written to `server.json` as `public_url`, and `cu login` prints `<public_url>/login` (falling back to `CU_PUBLIC_URL`, then `http://127.0.0.1:<port>/login`).
- **Host check.** The daemon answers `Host: localhost`, `127.0.0.1` or `[::1]` (any port, or no `Host` at all) on every route, and the configured public host on `/login` only. Any other `Host` gets `421 Misdirected Request`. That also stops DNS-rebinding pages reaching the API through a browser. A proxy must therefore pass the original host (`proxy_set_header Host $host;`), and a daemon behind a proxy **must** be started with `CU_PUBLIC_URL`, or the proxied `/login` is refused.
- The form posts back to the relative `/login`, and the daemon builds no absolute URLs, so `X-Forwarded-Proto` / `X-Forwarded-For` are not needed and are ignored. TLS ends at the proxy.
- `/login` is unauthenticated by design: anyone who can reach it can type credentials into whatever page the browser is showing. Publish it only while a login is in progress if that matters to you, or add `auth_basic` / an IP `allow` list in the proxy.

### Replicating the setup (as deployed on kraken)

kraken serves the daemon on port 8801 as `https://cucredentials.menustudioai.com/login`. For another daemon, swap in your own subdomain, server IP and port.

1. **DNS (Cloudflare).** Create an `A` record for the subdomain pointing at the server, DNS-only (grey cloud) so certbot's HTTP challenge and the TLS certificate are the server's own:

   ```sh
   curl -X POST "https://api.cloudflare.com/client/v4/zones/$CF_ZONE_ID/dns_records" \
     -H "Authorization: Bearer $CF_API_TOKEN" -H "Content-Type: application/json" \
     --data '{"type":"A","name":"cucredentials.menustudioai.com","content":"65.109.100.94","ttl":1,"proxied":false}'
   ```

2. **nginx vhost** (`/etc/nginx/sites-available/cucredentials.menustudioai.com`, symlinked into `sites-enabled`). Only `location = /login` (exact match, both GET and POST) is proxied; everything else is a redirect on :80 and a 404 on :443, so `/v1/*` is unreachable from outside:

   ```nginx
   server {
       listen 80;
       listen [::]:80;
       server_name cucredentials.menustudioai.com;
       location /.well-known/acme-challenge/ { root /var/www/certbot; }
       location = /login {
           proxy_pass http://127.0.0.1:8801;
           proxy_http_version 1.1;
           proxy_set_header Host $host;
           proxy_set_header X-Real-IP $remote_addr;
           proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
           proxy_set_header X-Forwarded-Proto $scheme;
       }
       location / { return 301 https://$host$request_uri; }
   }

   server {
       listen 443 ssl;
       listen [::]:443 ssl;
       server_name cucredentials.menustudioai.com;
       ssl_certificate     /etc/letsencrypt/live/cucredentials.menustudioai.com/fullchain.pem;
       ssl_certificate_key /etc/letsencrypt/live/cucredentials.menustudioai.com/privkey.pem;
       location = /login {
           proxy_pass http://127.0.0.1:8801;
           proxy_http_version 1.1;
           proxy_set_header Host $host;
           proxy_set_header X-Real-IP $remote_addr;
           proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
           proxy_set_header X-Forwarded-Proto $scheme;
       }
       location / { return 404; }
   }
   ```

3. **Certificate (Let's Encrypt).** Enable the :80 server block first, then issue with the nginx plugin (renewal is handled by certbot's timer), then add the :443 block and reload:

   ```sh
   sudo certbot certonly --nginx -d cucredentials.menustudioai.com
   sudo nginx -t && sudo systemctl reload nginx
   ```

4. **Daemon.** Start it with the public URL (in a systemd unit, add it to the `env` list):

   ```sh
   CU_PUBLIC_URL=https://cucredentials.menustudioai.com cu start --port 8801 --data /path/to/data
   ```

5. **Check.** `https://<host>/login` answers 200 with the form, `https://<host>/v1/status` answers 404 (nginx), and `cu login` prints the public URL.

## One-time codes and 2FA setup

`cu otp` covers the two moments a site asks for a one-time code, without a
live takeover of the browser:

- **Sign-in with an enrolled authenticator.** `cu otp code DOMAIN` mints the
  current TOTP code from the seed sealed in the domain's credential (`totp`
  field, an `otpauth://` URI or a bare base32 key; SHA-1/256/512, 4-8
  digits). With no seed on file, `cu otp request DOMAIN --wait` opens a
  hand-off and prints a `/vault/otp?t=…` link for the person: a temporary
  page, one random token per request, that takes their code (after the vault
  login if their session is out) and answers the waiting call the moment
  they send it. Links live ten minutes by default, are single-use, and the
  code is deleted as the agent picks it up — the vault UI's Codes view shows
  every hand-off and its outcome.
- **Enrollment (a setup QR on screen).** `cu otp capture --domain DOMAIN`
  finds the QR on the page, decodes it in pure Rust and seals the URI as the
  credential's `totp` field; the reply names issuer and account, never the
  seed. Enrollment's confirmation code is then just `cu otp code`. To put
  the same seed on the person's own authenticator, capture with
  `--request ID` from a `--kind setup` hand-off: the temporary page renders
  the same QR for them to scan.

Answering a hand-off is the person's side and stays UI-only: the daemon's
bearer token cannot look up, answer or render the QR of a request, the same
rule as secret-request approvals.

## SDK

```rust
let cu = cu::Client::new("127.0.0.1:8787", token);
println!("{}", cu.status()?);
```

Besides `status`, `navigate`, `snapshot`, `click` and `act`, the client
has `tabs` and `close_tab` for popups, `downloads` for finished files,
and `save_session`/`load_session` for profiles.
