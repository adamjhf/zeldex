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
nix develop -c cargo test
```

Usage:

1. Load the plugin:

```kdl
pane size=24 borderless=true {
    plugin location="file:/absolute/path/to/target/wasm32-wasip1/debug/zeldex.wasm" {
        poll_secs "1.2"
    }
}
```

Notes:

- built against Zellij `0.44.0`
- plugin gets pane cwd via Zellij plugin API, then runs one host refresh command on its timer to read recent Codex transcripts
- pane/thread matching is cwd + recency based; active bindings stay sticky until a newer matching transcript wins
- `waiting` is heuristic: last meaningful transcript entry was a tool call and the file has been quiet for at least 3s
- status comes from recent Codex transcripts under `~/.codex/sessions`; very old inactive threads age out with the transcript scan window
