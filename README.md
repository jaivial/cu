# cu — computer use for AI agents

`cu` runs one persistent Chromium profile behind a loopback-only HTTP server. Agents get a small CLI and Rust SDK for browser work; users enter credentials on a local HTML form so passwords never pass through chat or model-visible arguments.

See [docs/README.md](docs/README.md) for the API and [SKILL.md](SKILL.md) for agent usage.

```sh
cargo run -- start
cargo run -- status
```

This repository is local-only for now. Nothing is published.
