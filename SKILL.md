# cu skill

Use `cu` when an agent needs a persistent local browser session.

1. Start once: `cu start --data .cu`.
2. Check it: `cu status`.
3. Navigate with `cu navigate https://example.com`.
4. Read pages with `cu snapshot`, not `cu shot`: it is a few hundred bytes of
   `ref`-addressable elements instead of an image, so the model gets what it
   needs and pays almost nothing for it. Use `cu shot` only when pixels matter.
5. For credentials, run `cu login`, navigate to the site's sign-in page, then direct the user to the printed `/login` page. The daemon types what they enter into the browser. Never ask the user to paste a password into chat and never include passwords in tool arguments.
6. Save the session with `cu session save NAME`, and restore it later with `cu session load NAME`. A saved session keeps the login.

The bearer token is stored in `.cu/server.json`; keep that file private. The server binds to loopback only.
