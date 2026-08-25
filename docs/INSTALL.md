# Installation

[← README](../README.md)

### Homebrew (macOS / Linux — prebuilt, no compile)

```sh
brew tap tylern91/rqmd
brew trust tylern91/rqmd  # required on Homebrew ≥4.5
brew install rqmd
```

> The formula downloads a prebuilt binary — no Rust toolchain, cmake, or C++ compiler required.
> macOS arm64 and Linux x86_64 are supported. Other platforms: use the source build below.

### cargo install (source build, cross-platform)

Requires Rust stable ≥1.88 (the MSRV, enforced by a pinned CI job), cmake ≥3.14, and a C/C++ toolchain (builds llama.cpp from source).

```sh
cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli
```

On Linux, Metal is not available — prefix with `LLAMA_METAL=0`:

```sh
LLAMA_METAL=0 cargo install --git https://github.com/tylern91/rqmd --locked rqmd-cli
```

### Prebuilt binary (manual download)

Download from the [latest GitHub Release](https://github.com/tylern91/rqmd/releases/latest),
then verify and install. Asset names carry the version (e.g.
`rqmd-v0.8.0-aarch64-apple-darwin.tar.gz`), so resolve the tag first rather than guessing
an unversioned filename:

```sh
VERSION="$(curl -fsSL https://api.github.com/repos/tylern91/rqmd/releases/latest | grep -m1 '"tag_name"' | cut -d'"' -f4)"

# macOS arm64
curl -fLO "https://github.com/tylern91/rqmd/releases/download/${VERSION}/rqmd-${VERSION}-aarch64-apple-darwin.tar.gz"
curl -fLO "https://github.com/tylern91/rqmd/releases/download/${VERSION}/rqmd-${VERSION}-aarch64-apple-darwin.tar.gz.sha256"
shasum -a 256 -c "rqmd-${VERSION}-aarch64-apple-darwin.tar.gz.sha256"
tar -xf "rqmd-${VERSION}-aarch64-apple-darwin.tar.gz"
install -m 0755 rqmd ~/.local/bin/rqmd   # or /usr/local/bin/rqmd

# Linux x86_64
curl -fLO "https://github.com/tylern91/rqmd/releases/download/${VERSION}/rqmd-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
curl -fLO "https://github.com/tylern91/rqmd/releases/download/${VERSION}/rqmd-${VERSION}-x86_64-unknown-linux-gnu.tar.gz.sha256"
shasum -a 256 -c "rqmd-${VERSION}-x86_64-unknown-linux-gnu.tar.gz.sha256"
tar -xf "rqmd-${VERSION}-x86_64-unknown-linux-gnu.tar.gz"
install -m 0755 rqmd ~/.local/bin/rqmd
```

### From source (recommended while in development)

Requirements: Rust stable (≥1.88, the MSRV, enforced by a pinned CI job), cmake ≥3.14 (cmake 4.x supported), Xcode Command Line Tools (macOS) or `build-essential` (Linux).

```sh
# Clone the repo
git clone https://github.com/tylern91/rqmd
cd rqmd

# Development build (fast, debug symbols)
cargo build -p rqmd-cli

# Optimized release binary (~60MB, fat LTO + stripped)
cargo build --profile dist -p rqmd-cli
# → target/dist/rqmd

# Install to ~/.cargo/bin/ (content-aware: rebuilds only when source changed)
./scripts/install.sh
```

> **Why not `cargo install --path`?** `cargo install` skips reinstalling when the crate version
> is unchanged, so source changes without a version bump are silently ignored. `scripts/install.sh`
> uses `cargo build`'s fingerprinting instead — it rebuilds only when something actually changed,
> then copies the fresh binary into `~/.cargo/bin/`. No `--force`, no manual version bump.

### With ONNX Runtime backend (CoreML / CUDA / DirectML)

```sh
cargo build --profile dist -p rqmd-cli --features ort-backend
# or install directly:
./scripts/install.sh --features ort-backend
```

This downloads the ONNX Runtime library at build time. The resulting binary
supports CoreML (Apple Neural Engine on macOS), CUDA (NVIDIA GPU), and DirectML
(Windows GPU) in addition to the CPU fallback.

### Linux

```sh
sudo apt-get install cmake build-essential
cargo build -p rqmd-cli
```

For a fully static MUSL binary (no glibc dependency):

```sh
rustup target add x86_64-unknown-linux-musl
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --profile dist -p rqmd-cli --target x86_64-unknown-linux-musl
```
