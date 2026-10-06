# Agent instructions

`cu` is a local browser session service for agents. Keep browser profiles and `server.json` private; do not commit them or publish them.

## Safe login

Credentials must be entered by the user in `http://127.0.0.1:8787/login`. Agents may open or describe that link, but must not request, log, echo, or pass passwords through chat, prompts, command arguments, or API JSON.

## Development

- Run `cargo fmt --check` and `cargo test` before each handoff.
- Keep the daemon loopback-only.
- Preserve session profile data and avoid destructive commands.
