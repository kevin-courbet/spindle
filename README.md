# Spindle

Backend daemon for [Threadmill](https://github.com/kevin-courbet/threadmill). A single Rust binary that manages git worktrees, tmux sessions, chat agent processes, and terminal I/O relay over WebSocket JSON-RPC.

Runs on Linux (primary: Ubuntu 22.04 / WSL2) and macOS.

## Protocol

Shared schema: `threadmill/protocol/threadmill-rpc.schema.json` (git submodule — only needed by `codegen/generate.sh`). Generated Rust bindings live at `src/protocol.rs` and are committed.

## Install (release binary)

Download from the [latest release](https://github.com/kevin-courbet/spindle/releases/latest). One archive per target triple, each containing `spindle` + `threadmill-cli`:

```bash
VERSION=v0.1.0
TRIPLE=x86_64-unknown-linux-gnu   # or aarch64-unknown-linux-gnu, {aarch64,x86_64}-apple-darwin
curl -sSLO https://github.com/kevin-courbet/spindle/releases/download/${VERSION}/spindle-${VERSION}-${TRIPLE}.tar.gz
curl -sSLO https://github.com/kevin-courbet/spindle/releases/download/${VERSION}/spindle-${VERSION}-${TRIPLE}.tar.gz.sha256
shasum -a 256 -c spindle-${VERSION}-${TRIPLE}.tar.gz.sha256

tar -xzf spindle-${VERSION}-${TRIPLE}.tar.gz
mkdir -p ~/.local/bin
install spindle-${VERSION}-${TRIPLE}/spindle spindle-${VERSION}-${TRIPLE}/threadmill-cli ~/.local/bin/
```

Then register as a systemd user service (Linux):

```bash
mkdir -p ~/.config/systemd/user
cp spindle.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spindle
journalctl --user -u spindle -f    # follow logs
```

Linux glibc floor: 2.31 (Ubuntu 20.04+). macOS binary is unsigned; run via Terminal to bypass Gatekeeper on first launch.

## Configuration

| Env var | Default (Linux) | Default (macOS) | Purpose |
|---|---|---|---|
| `SPINDLE_ADDR` | `127.0.0.1:19990` | same | WebSocket bind address |
| `SPINDLE_WORKSPACE_ROOT` | `/home/wsl/dev` | `$HOME/dev` | Root for new clones + worktrees |
| `OPENCODE_BIN` | `/home/wsl/.bun/bin/opencode` | `opencode` (PATH) | Opencode agent binary |
| `RUST_LOG` | `info` | same | Tracing level |

## Build from source

Prerequisites:

- Rust stable (managed via `rust-toolchain.toml`).
- Submodule: `git clone --recurse-submodules`, or `git submodule update --init` after cloning. Only needed if you want to regenerate `src/protocol.rs` via `codegen/generate.sh` (requires `python3` + `jq`).

```bash
cargo build --release                 # native build for the host
cargo test --lib --locked             # unit tests (integration tests require a real threadmill fixture)
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

### Cross-compile Linux binaries on macOS

Uses [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) and Zig for a Docker-free cross-toolchain.

```bash
brew install zig
cargo install cargo-zigbuild --locked
rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu

cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.31 --bin spindle --bin threadmill-cli
```

The `.2.31` suffix pins glibc — the resulting binary runs on any Linux with glibc ≥ 2.31.

## CI & releases

- **CI** (`.github/workflows/ci.yml`): every push / PR runs `cargo fmt --check`, `cargo clippy -D warnings`, `cargo check`, `cargo test --lib`, and `cargo-deny check` on a macOS runner.
- **Release** (`.github/workflows/release.yml`): push a tag matching `v*` and GitHub Actions builds all four targets via one macOS runner (native for mac targets, `cargo-zigbuild` for Linux), packages `spindle` + `threadmill-cli` per target, and publishes a GitHub release with matching `.sha256` files. Also usable via `workflow_dispatch` for dry-runs.

## Release history

See [releases](https://github.com/kevin-courbet/spindle/releases).

## License

MIT — see [LICENSE](LICENSE).
