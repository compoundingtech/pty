# pty with Nix

pty ships a [flake](flake.nix) with a package and a development shell. Both need
[Nix](https://nixos.org/download) with flakes enabled. Works on macOS and Linux.

## Install the CLI

Install `pty` into your Nix profile:

```sh
nix profile install github:compoundingtech/pty
```

This builds the CLI from the flake's default package and puts `pty` on your
`PATH`. Upgrade later with `nix profile upgrade`, remove with
`nix profile remove`.

To run it once without installing:

```sh
nix run github:compoundingtech/pty
```

## Development shell

For hacking on pty itself, drop into the flake's dev shell — it provides the
toolchain needed to build the `node-pty` native addon (Node.js, Python, and
pkg-config):

```sh
nix develop github:compoundingtech/pty
```

Or, from a clone of this repo:

```sh
git clone https://github.com/compoundingtech/pty
cd pty
nix develop          # enters the dev shell with node, python3, pkg-config
npm install
npm run build
```

The flake's `flake.nix` is the source of truth for both the package and the dev
shell — this document only describes how to use them.
