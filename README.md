# zeldex

Vertical Zellij sidebar for Codex tabs.

Current MVP:

- vertical tabs in a left sidebar
- click or scroll to switch tabs
- Codex-only
- status source is recent Codex transcripts under `~/.codex/sessions`
- statuses are `idle`, `running`, `waiting`, `done`
- marks inactive tabs unread when a tracked pane flips into `waiting` or `done`

Dev loop:

```sh
nix develop -c cargo build --target wasm32-wasip1 --bin zeldex
nix develop -c cargo build --target aarch64-apple-darwin --features native --bin zeldex-status
nix develop -c cargo test --features native
```

Usage:

1. Load the plugin:

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
- plugin gets pane pid via Zellij plugin API, then runs one-shot `zeldex-status` refreshes on its timer
- `zeldex-status` only tracks panes with a live Codex descendant process or a cached Codex binding from earlier polls
- live Codex panes prefer exact transcript files discovered from descendant processes; cwd matching is only a fallback after a pane is known to be Codex-backed
- `waiting` is heuristic: last meaningful transcript entry was a tool call and the file has been quiet for at least 3s
- thread metadata is cached on disk, so long-lived `waiting` / `done` panes keep their status without rescanning full history every poll
