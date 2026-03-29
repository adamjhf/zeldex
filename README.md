# zeldex

Vertical Zellij sidebar for Codex tabs.

Current MVP:

- vertical tabs in a left sidebar
- click or scroll to switch tabs
- Codex-only
- status source is Codex app-server runtime state
- statuses are `running`, `waiting`, `done`
- marks inactive tabs unread when a tracked pane flips into `waiting` or `done`

Dev loop:

```sh
nix develop -c cargo build --target wasm32-wasip1 --bin zeldex
nix develop -c cargo build --target aarch64-apple-darwin --features native --bin zeldex-codex --bin zeldex-status
nix develop -c cargo test
```

Usage:

1. Start tracked Codex panes with `zeldex-codex` instead of `codex`:

```sh
target/aarch64-apple-darwin/debug/zeldex-codex
target/aarch64-apple-darwin/debug/zeldex-codex resume
```

2. Load the plugin:

```kdl
pane size=24 borderless=true {
    plugin location="file:/absolute/path/to/target/wasm32-wasip1/debug/zeldex.wasm" {
        poll_secs "1.2"
        status_cmd "/absolute/path/to/target/aarch64-apple-darwin/debug/zeldex-status"
    }
}
```

Notes:

- built against Zellij `0.44.0`
- `zeldex-codex` starts a per-process `codex app-server` on loopback and launches `codex --remote`
- plugin gets pane pid via Zellij plugin API, then runs one-shot `zeldex-status` refreshes on its timer
- `zeldex-status` maps pane pid -> wrapper runtime file, then queries the relevant app-server over websocket
