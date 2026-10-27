# autopin

One package that fails to build should not hold back every other update. autopin
runs after `nix flake update`. It builds your outputs, and any package that fails
is pinned to the nixpkgs revision it last built at. Everything else moves to the
new lock. On later runs it retries each pin once its input has moved, and removes
the pin when the package builds again.

## How it works

- `failures.json` lives next to `flake.lock` and is owned by the tool. Each pin
  records the flake input, the top-level attr, and the `locked` node it last
  built at, copied from the last good `flake.lock`. That keeps
  `builtins.fetchTree` pure.
- `lib.mkOverlay { failures = ./failures.json; input = "nixpkgs"; }` swaps
  each pinned attr for its copy from the old revision. With no pins it is a
  no-op.
- `autopin run` builds with `--keep-going` and reads the failures from the log:
  - **Build failure:** walks up the derivation graph from the failed drv to the
    packages your configuration uses directly, and pins those rather than the
    library underneath.
  - **Eval failure** (missing, removed, or broken/insecure attr): pins the attr
    if the last good revision provides it.
- It gives up (exit 1, `failures.json` untouched) when a failure is in
  configuration glue, hits the denylist (kernel, glibc, systemd, nix, stdenv,
  ...), needs too many new pins, or keeps failing after `--max-rounds`.

## Usage

```
autopin run --flake . --old-lock published.lock \
  --output home=nixosConfigurations.home.config.system.build.toplevel \
  --pkgs nixpkgs=nixosConfigurations.home.pkgs
autopin status --failures failures.json
autopin clear --failures failures.json nixpkgs obs-studio
```

## Tests

```
cargo test
nix flake check
cargo build && tests/e2e.sh
```

`tests/e2e.sh` runs against a toy nixpkgs whose history breaks a library and
removes an attr, then fixes both. It checks that the pins appear, survive while
they are still needed, and clear by themselves.
