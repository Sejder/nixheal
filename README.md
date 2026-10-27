# autopin

One package that fails to build should not hold back every other update.
autopin updates your flake and builds your hosts. Any package that fails is
pinned to the nixpkgs it last built with, and every other package moves on. The
pin is removed by itself once the package builds again. A host that pinning
cannot fix, because of a kernel, glibc or a module error, stays on its last good
lock while the other hosts update.

## Setup

```nix
# flake.nix
inputs.autopin.url = "git+https://gitlab.sejder.dev/Sejder/autopin.git";

# in every NixOS host
imports = [ inputs.autopin.nixosModules.default ];
programs.autopin = {
  enable = true;
  pinsFile = ./pins.json;      # autopin creates it
  hosts = [ "ideapad" ];       # what this machine builds when autopin runs here
  nixpkgs.nixpkgs-unstable = { # extra package sets, pinned too
    source = inputs.nixpkgs-unstable;
    argName = "unstable-pkgs";
  };
};
```

`homeManagerModules.default` has the same options for Home Manager. Inside
NixOS it follows the host's settings. Standalone, it reads its own `homes`
entry and switches with `nh home switch`.

Options:

| Option | Meaning |
|---|---|
| `enable` | apply this machine's pins; also install autopin when `hosts`/`homes` is set |
| `pinsFile` | the flake's `pins.json` |
| `flake` | the checkout to update, default `programs.nh.flake` |
| `hosts`, `homes` | configurations to build and pin when autopin runs here |
| `warnAfterDays` | log a pin at warning level after this many days (14) |
| `deny` | extra attrs never to pin (`cuda*`) |
| `nixpkgs.<input>` | import another nixpkgs with pins; `argName` passes it to modules |

The settings are written to `/etc/autopin.json` or `~/.config/autopin.json`,
so autopin keeps working when the updated flake no longer evaluates.

## Use

```
autopin update [INPUT...]   # nix flake update, pin/unpin, commit, nh switch
autopin switch              # pin/unpin against the current lock, commit, nh switch
autopin status              # pins and held hosts, with their age
autopin why <attr>          # failed drv, path up to the package, build log tail
autopin pin <attr>          # pin by hand at the current lock, kept until cleared
autopin clear <attr>        # remove a pin; --held releases a held host
```

- `update` and `switch` take `--dry-run`, `--no-commit`, `--no-switch` and `--only <name>`.
- They run in a dirty tree and commit only `flake.lock` and `pins.json`.
- If the machine's own configuration cannot be fixed, both files are put back. Nothing is committed and nothing is switched.

## How it decides

- **Build failure:** it walks up the derivation graph, inside that host's closure, to the packages the configuration uses directly. It pins those, never a library on its own.
- **Eval failure** (missing, removed, broken or insecure attr): it pins the attr if the last good nixpkgs provides it.
- **Retries:** a pin is retried whenever its input has moved, and removed once the package builds. Manual pins are never retried.
- **Holding:** a host is held instead when any of these is true:
  - the failure is in configuration glue;
  - it hits the denylist (kernel, glibc, systemd, nix, stdenv, gcc, ...);
  - it fails at the old revision too;
  - it needs more than `--max-new-pins`.

`pins.json` holds one entry per host:

```json
{ "version": 2,
  "hosts": { "ideapad": { "lockSha256": "...", "lockLastModified": 1793083946,
                          "pins": { "nixpkgs": { "sbctl": { "locked": {...}, "since": "2026-10-27",
                                                            "reason": "build", "failedDrv": "...", "trail": [...] } } } },
             "pi": { "held": { "since": "...", "reason": "...", "lock": {...} } } } }
```

Each run writes only the entries it built. When two runs disagree, the entry
computed from the newer lock wins.

## Tests

```
cargo test
nix flake check
cargo build && tests/e2e.sh
```
