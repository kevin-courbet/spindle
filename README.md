# Spindle

Backend daemon for [Threadmill](https://github.com/kevincourbet/threadmill). Manages git worktrees, tmux sessions, and terminal I/O relay on beast (WSL2).

## Protocol

See `protocol/threadmill-rpc.schema.json` for the JSON-RPC 2.0 contract shared with the Threadmill macOS app.

## Build & Run

```bash
cargo build --release
cp target/release/spindle ~/.cargo/bin/
spindle  # listens on 127.0.0.1:19990
```

## systemd (if available)

```bash
cp spindle.service ~/.config/systemd/user/
systemctl --user enable --now spindle
```
