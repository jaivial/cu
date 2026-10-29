# cu skill

Use `cu` when an agent needs a persistent local browser session.

1. Start once: `cu start --data .cu`.
   If a site says the browser lacks WebGL/WebGL2 (e.g. CapCut's
   `/incompatibility` page), restart with `cu start --webgl` (or
   `CU_WEBGL=1`): software WebGL2, more CPU, off by default.
   The browser runs with a window on an X display (cu starts its own Xvfb when
   the machine has none) and with a windowed User-Agent and no
   `navigator.webdriver`, because sites behind Cloudflare Turnstile answer a
   headless browser with "verifying you are not a bot" instead of the page.
   `CU_HEADLESS=1` asks for `--headless=new` back (and those sites will
   challenge), `CU_DISPLAY=:N` picks the display.
2. Check it: `cu status`.
3. Navigate with `cu navigate https://example.com`. A page command without
   `--tab` works in the tab this session already has, opening one the first
   time: the commands of one agent -- `navigate`, then `snapshot`, then a
   `click` -- share one page. `--new-tab` opens a page of this call's own
   instead (what a test that wants its own page asks for). If the reply says
   `"settled":false`, the page was still loading when the three-second wait
   ended: take a snapshot before acting. A navigation that cannot happen is a
   `navigation failed: ...` error, not silence.
4. Read pages with `cu snapshot`, not `cu shot`: it is a few hundred bytes of
   `ref`-addressable elements instead of an image, so the model gets what it
   needs and pays almost nothing for it. Use `cu shot` only when pixels matter.
   For images people will see, shape the capture: `cu shot out.png --width 600
   --scale 2 --selector '.card' --padding 12` (or `--ref eN`) is a sharp 2x clip
   of one element; the viewport is restored afterwards.
   Lines under `- frame: <url>` are inside an iframe; their refs work like any
   other -- click and type reach into the frame. When the answer is words
   rather than something to act on (prose, an API response, a message), read
   `cu text` -- the snapshot only lists what is clickable. `cu console --level warn+`
   shows console warnings, errors, uncaught exceptions, CSP and network failures.
5. Act on what the snapshot shows by ref: `cu click e3`, `cu type e2 "text"
   [--submit]`. For several steps, send them at once and get the resulting
   page back in the same call:
   `cu act '[{"do":"type","ref":"e2","text":"Ada"},{"do":"click","ref":"e5"}]'`.
   A reply with `"ok":false` says which action failed and why; a stale ref
   means "take a new snapshot". Two more result fields to watch: an action
   that starts a download answers `"download":"name"` (list finished files
   with `cu downloads`), and one that opens a window answers `"popup":true`
   (see `cu tabs`).
6. Credentials. When you have them (a test account, a secret the user gave you, a vault entry, `cu otp code`), type them like any field: `cu type e4 "$PASSWORD"` or a `type` step in `cu act`. Read them from a `chmod 600` secrets file into a variable rather than writing the literal on the command line, and never repeat one in your answer; cu never echoes it back (no reply carries a field's value). When the user should type their own, run `cu login`, navigate to the site's sign-in page, then direct them to the printed `/login` page; the daemon types what they enter.
7. Popups and extra tabs: `cu tabs` lists them (the default is flagged);
   any page command takes `--tab ID` to run there, and `cu tabs close ID`
   dismisses one. A tab a session opened is its own: another agent never
   lands in it. Never act on a popup guessed from `cu tabs` output of an
   older snapshot -- ids and urls come from the last `cu tabs`.
8. QA references. When a check fails, pin it to the element instead of
   describing it in prose: `cu mark e7 "overlaps the footer at 375px"
   --severity major` (blocker|major|minor|nit; `--selector CSS` instead of a
   ref). `cu marks --md` is the report for the developer: per mark the note,
   a unique selector (`a >>> b` steps into a shadow root), test id,
   landmarks, box and styles, and on a dev build the component and its
   source file (`src/LoginForm.tsx:42:7`). `cu inspect eN` shows the same
   for one element without marking it. For evidence, `cu shot qa.png
   --marks` boxes the marks numbered like the report, and `--annotate
   all` (or `e1,e4`) labels snapshot refs on the image, so pixels and refs
   point at each other. Marks live in the daemon per tab until `cu marks
   clear`; field values are never recorded.
9. Save the session with `cu session save NAME`, and restore it later with `cu session load NAME`. A saved session keeps the login.
10. A one-time-code (2FA) page is not a full takeover any more. If the vault
    holds a seed for the site, mint the code yourself:
    `cu otp code example.com` (or `--credential ID`) and type it in. If not,
    `cu otp request example.com --reason "GitHub login" --wait` prints a
    short link — send the user that link, nothing else; the page asks their
    master password if their vault session is out, then takes the code, and
    your `--wait` returns the moment they send it. Cancel with
    `cu otp cancel ID` if you take another path. Never ask the user to paste
    a code into chat: the link exists so codes (and seeds) never pass
    through a conversation.
11. Setting up 2FA on a site (a setup QR is on screen): capture it instead of
    photographing it — `cu otp capture --domain example.com` decodes the QR
    from a tab you name explicitly (`--tab T`) and that must actually be on
    that domain (it refuses mismatched pages, mismatched issuers and pages
    with several different setup QRs).
    If the credential already has a live seed, capture refuses; only a
    deliberate `--replace` overwrites it, and the reply says so. Never log,
    print or relay the seed or minted codes beyond typing them in.
    Collection is acknowledged: `cu otp wait` confirms receipt to the vault
    before the code is printed, and the vault wipes it on that ACK. The
    wait keeps its collect token (and a retrieved code) in a 0600 state
    file (atomically written, symlink-safe), so a crashed or killed wait is
    genuinely resumed by running `cu otp wait ID` again while the vault's
    delivery window (five minutes - retention, not code validity; a TOTP
    code lives ~30 seconds) is open. State past ten minutes is deleted
    when a wait next touches the directory, never resumed, and a definitive
    vault refusal ends the wait instead of retrying forever. A crashed wait
    nobody reruns can leave its 0600 file behind (an expired token, a dead
    code) until the next `cu otp` run - same-UID, loopback-only hygiene,
    not a guaranteed cleanup. Never move the token or the code out of that
    file by hand.
    seals the seed into the domain's credential as a secret `totp` field and
    answers with only the issuer and account it stored. From then on
    `cu otp code` mints the codes, including the enrollment confirmation one.
    If the user wants the same seed on their phone, capture with
    `--request ID` from a `cu otp request --kind setup` and send them the
    link: its page shows the QR to scan. A captured seed is a permanent
    secret: never print it, screenshot it into a report, or paste it
    anywhere — the capture reply never carries it, keep it that way.

12. Over MCP. When the harness has the `cu mcp` server configured (tools named
    like `mcp__cu__navigate`), the same flow is tools instead of commands:
    `navigate`, `snapshot`, `click`, `type`, `act`, `screenshot`, `tabs`,
    `console`, `inspect`, `mark`/`marks`, `handoff`, `login_url`... The
    connection is the session: its page tools share one tab (pass `tab` for a
    popup), and the tab is closed when the connection ends. No MCP tool takes a
    password or a code: `login_url` for sign-ins, and the vault and `cu otp`
    stay on the CLI.

The bearer token is stored in `.cu/server.json`; keep that file private. The server binds to loopback only.
