set shell := ["bash", "-euo", "pipefail", "-c"]
# per machine settings like ATLAS_HOME / ATLAS_REMOTE_HOST, see .env.example
set dotenv-load := true

# build with the toolchain pinned in rust-toolchain.toml even when another
# rust (e.g. homebrew) comes first on PATH. no rustup = whatever is on PATH.
toolchain_bin := `command -v rustup >/dev/null 2>&1 && dirname "$(rustup which rustc 2>/dev/null)" 2>/dev/null || true`
export PATH := if toolchain_bin == "" { env("PATH") } else { toolchain_bin + ":" + env("PATH") }

# ssh host for the build-remote recipes, e.g. `ATLAS_REMOTE_HOST=me@buildbox just build-remote`
remote_host := env("ATLAS_REMOTE_HOST", "")
# folder on the remote host the source is synced into (relative to its home)
remote_dir := env("ATLAS_REMOTE_DIR", "atlas-build")

# list recipes
default:
    @just --list

# show which rust the recipes use
toolchain:
    @rustc --version && cargo --version && echo "from: $(command -v cargo)"

# run atlas (optimized), e.g. `just run --selftest`
run *args:
    cargo run --release -- {{ args }}

# unit, parity and end to end tests
test:
    cargo test --locked

# format the code
format:
    cargo fmt

# fail if anything isnt formatted
format-check:
    cargo fmt --check

# clippy with warnings as errors
lint:
    cargo clippy --all-targets --locked -- -D warnings

# optimized build -> target/release/atlas
build: build-release

# optimized build -> target/release/atlas
build-release:
    cargo build --release --locked

# debug build -> target/debug/atlas
build-debug:
    cargo build --locked

# optimized build on $ATLAS_REMOTE_HOST -> target/remote/release/atlas
build-remote: build-remote-release

# optimized build on $ATLAS_REMOTE_HOST -> target/remote/release/atlas
build-remote-release: (_remote "release")

# debug build on $ATLAS_REMOTE_HOST -> target/remote/debug/atlas
build-remote-debug: (_remote "debug")

# sync the source to the remote host, cargo build there, copy the binary back
_remote profile:
    #!/usr/bin/env bash
    set -euo pipefail
    host="{{ remote_host }}"
    dir="{{ remote_dir }}"
    if [ -z "$host" ]; then
        echo "set ATLAS_REMOTE_HOST to the ssh host to build on, e.g. ATLAS_REMOTE_HOST=me@buildbox just build-remote" >&2
        exit 1
    fi
    flag=""
    if [ "{{ profile }}" = "release" ]; then flag="--release"; fi
    out="target/remote/{{ profile }}"

    echo "==> syncing source to $host:$dir"
    ssh "$host" "mkdir -p '$dir'"
    # never ship local data or secrets (config.json holds usenet passwords)
    rsync -az --delete \
        --exclude '/target/' --exclude '/.git/' --exclude '/SABnzbd-5.0.4/' --exclude '/img/' \
        --exclude 'config.json*' --exclude 'atlas.db*' --exclude '*.log' --exclude '*.log.old' \
        --exclude 'status.json' --exclude 'stats.json' --exclude '*.pid' --exclude '*.tmp' \
        ./ "$host:$dir/"

    echo "==> cargo build --locked $flag on $host"
    ssh "$host" "cd '$dir' && if [ -f \"\$HOME/.cargo/env\" ]; then . \"\$HOME/.cargo/env\"; fi && cargo build --locked $flag"

    mkdir -p "$out"
    scp -q "$host:$dir/target/{{ profile }}/atlas" "$out/atlas"
    chmod +x "$out/atlas"
    echo "==> $out/atlas built on $(ssh "$host" uname -sm)"
