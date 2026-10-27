# nixheal

One package that fails to build should not hold back every other update.
nixheal updates your flake and builds your hosts. Any package that fails is
pinned to the nixpkgs it last built with, and every other package moves on. The
pin is removed by itself once the package builds again. A host that pinning
cannot fix, because of a kernel, glibc or a module error, stays on its last good
lock while the other hosts update.

## Setup

```nix
# flake.nix
inputs.nixheal.url = "git+https://gitlab.sejder.dev/Sejder/nixheal.git";

# in every NixOS host
imports = [ inputs.nixheal.nixosModules.default ];
programs.nixheal = {
  enable = true;
  pinsFile = ./pins.json;      # nixheal creates it
  hosts = [ "ideapad" ];       # what this machine builds when nixheal runs here
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
| `enable` | apply this machine's pins; also install nixheal when `hosts`/`homes` is set |
| `pinsFile` | the flake's `pins.json` |
| `flake` | the checkout to update, default `programs.nh.flake` |
| `hosts`, `homes` | configurations to build and pin when nixheal runs here |
| `warnAfterDays` | log a pin at warning level after this many days (14) |
| `deny` | extra attrs never to pin (`cuda*`) |
| `quiet` | always run as `--quiet` |
| `nixpkgs.<input>` | import another nixpkgs with pins; `argName` passes it to modules |

The settings are written to `/etc/nixheal.json` or `~/.config/nixheal.json`,
so nixheal keeps working when the updated flake no longer evaluates.

## Use

```
nixheal update [INPUT...]   # nix flake update, pin/unpin, commit, nh switch
nixheal switch              # pin/unpin against the current lock, commit, nh switch
nixheal status              # pins and held hosts, with their age
nixheal status --json       # one flat JSON object for a dashboard widget
nixheal why <attr>          # failed drv, path up to the package, build log tail
nixheal pin <attr>          # pin by hand at the current lock, kept until cleared
nixheal clear <attr>        # remove a pin; --held releases a held host
nixheal merge <file>        # fold another pins.json in; per host, the newer lock wins
```

- `update` and `switch` take `--dry-run`, `--no-commit`, `--no-switch` and `--only <name>`.
- `-q` / `--quiet` (or `programs.nixheal.quiet = true`) prints only pins, unpins, holds, warnings and errors. nh's own output is unchanged.
- They run in a dirty tree and commit only `flake.lock` and `pins.json`.
- If the machine's own configuration cannot be fixed, both files are put back. Nothing is committed and nothing is switched.
- With `--no-switch` nothing local needs protecting, so the machine's own host is held like any other. That is the mode for a fleet build host.
- `--gc-roots DIR` keeps every system that built alive under `DIR/<host>` (and `DIR/home-<name>`), so a binary cache on that machine keeps serving them. A held host's previous root is left alone.

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

## Dashboards

`nixheal status --json` prints one flat object a homepage-dashboard
`customapi` widget (or anything else) can read:

```json
{"status":"PINNED","pins":1,"held":0,"stale":0,"oldest":"home: open-webui, 3d","oldest_days":3,
 "list":"home: open-webui (3d)","warn_after_days":14,"generated":"2026-10-30"}
```

`items` holds one row per pin or held host (`name`, and a `label` such as
`3d, eval` or `2d, held`, with `, STALE` past `warnAfterDays`), for a
`dynamic-list` widget. With nothing pinned it holds a single `OK` row.

`status` is the most urgent of `STALE` (a pin or hold older than
`warnAfterDays`), `HELD`, `PINNED` and `OK`. Ages are computed when it runs,
so regenerate it regularly, e.g. from the timer that runs nixheal.
