# Build Instructions

## Rust Compiler

Install `rustc` and `cargo` following [these instructions](https://rust-lang.org/tools/install/).

## Kernel Build Dependencies

The node links against `bitcoinkernel`, which requires the same toolchain and system libraries as Bitcoin Core. See the Bitcoin Core build docs for full details:

- [Linux build guide](https://github.com/bitcoin/bitcoin/blob/v31.0/doc/build-unix.md)
- [macOS build guide](https://github.com/bitcoin/bitcoin/blob/v31.0/doc/build-osx.md)
- [Dependencies reference](https://github.com/bitcoin/bitcoin/blob/v31.0/doc/dependencies.md)

#### Ubuntu / Debian

```bash
sudo apt-get install build-essential cmake pkgconf python3 libevent-dev libboost-dev capnproto libcapnp-dev
```

#### Arch Linux

```bash
sudo pacman -S --needed base-devel cmake pkgconf python libevent boost capnproto
```

#### NixOS

Drop into a shell with the required dependencies:

```bash
nix-shell -p gcc cmake pkg-config python3 libevent boost capnproto
```

#### macOS

Install the Xcode Command Line Tools and the dependencies via [Homebrew](https://brew.sh):

```bash
xcode-select --install
brew install cmake pkgconf boost libevent capnp
```
