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
plus the headings. It is a single CDP round trip and a few hundred bytes -- use
it instead of reading the DOM or OCRing an image.

```json
{"snapshot":"- url: https://example.test/\n- page: Example\n- ref=e1 link \"More\"\n"}
```

Screenshots are JPEG by default because a model reads a lossy frame just as
well and the capture, encode and transfer all shrink. `GET
/v1/screenshot?format=png` gives a lossless PNG back; `cu shot FILE` writes
either, naming the file to match.

Navigation waits for the page to be interactive and stable before it answers,
bounded to three seconds, and reports what it spent in `settled_ms`. A page
that keeps a stream open for ever -- SSE, websocket, long poll -- therefore
costs at most three seconds instead of hanging the action.

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

### Contexts

Add `?context=NAME` to navigate, snapshot, screenshot or act to work in an
isolated browser context of the same browser (own cookies and storage),
created on first use. `GET /v1/contexts` lists them and `DELETE
/v1/contexts/NAME` closes one. Stopping `cu` closes its browser.

Latency for every one of these is tracked in [BENCHMARKS.md](BENCHMARKS.md),
including a comparison with agent-browser and Playwright MCP.

The browser executable defaults to `chromium`; set `CU_BROWSER` to an alternate binary. Browsers are launched headless, which is what an agent usually wants; set `CU_HEADLESS=0` to attach a display instead. `CU_DATA_DIR` changes the default data directory and `CU_CDP_PORT` the DevTools port (default `9222`). Do not expose this server beyond localhost without adding TLS and an access-control layer.

## Login

`POST /login` takes `username` and `password` form fields. The daemon types them into the page the browser is showing and submits the form, which is how the credentials reach the site without ever passing through an agent. The password is not written to disk, not logged and not included in the response; the reply only says whether it could be typed. Navigate to the sign-in page first, or the submission has nowhere to go.

## SDK

```rust
let cu = cu::Client::new("127.0.0.1:8787", token);
println!("{}", cu.status()?);
```
