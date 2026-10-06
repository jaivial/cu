# cu — computer use for AI agents

`cu` runs one persistent Chromium profile behind a loopback-only HTTP server. Agents get a small CLI and Rust SDK for browser work; users enter credentials on a local HTML form so passwords never pass through chat or model-visible arguments.

See [docs/README.md](docs/README.md) for the API and [SKILL.md](SKILL.md) for agent usage.

```sh
cargo run -- start
cargo run -- status
```

The landing page is at <https://jaivial.github.io/cu/>.

## Security

The daemon binds to loopback only. Its bearer token is generated per data
directory and stored in `.cu/server.json`, which is gitignored and must never be
committed or published. Credentials are typed into the page by the daemon from
the local login form; they are never passed to the model, logged, or returned.
