# Agent instructions

`cu` is a local browser session service for agents. Keep browser profiles and `server.json` private; do not commit them or publish them.

## Safe login

Agents may type a password or a one-time code they were given (test accounts, a secret the user handed over, the vault, `cu otp code`) with `type`/`act`, on the CLI or over MCP. They must not echo it back in chat or log it, and cu must never return a typed secret (no reply carries a field's value; the snapshot never names a password field by it). When the user should enter their own credentials, send them `http://127.0.0.1:8787/login` (`cu login`, MCP `login_url`).

## Development

- Run `cargo fmt --check` and `cargo test` before each handoff.
- Keep the daemon loopback-only.
- Preserve session profile data and avoid destructive commands.
