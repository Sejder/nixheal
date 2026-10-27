# nixheal

Keep a NixOS flake updating when single packages break.

When one package in a `nix flake update` fails to build, the whole system
update fails, and every other security and bug fix waits with it until you
notice, disable the package or write an overlay. nixheal does that for you,
and undoes it by itself:

- it updates your flake and builds your hosts;
- a package that fails is **pinned** to the nixpkgs it last built with, and
  everything else moves to the new lock;
- on every later update the pin is retried, and it is **removed** as soon as
  the package builds again;
- a host that pinning cannot fix (a kernel, glibc, a NixOS module error) is
  **held** on its last good lock while the other hosts move on.

```
$ nixheal switch --update
nixheal: round 1: evaluating and building host laptop with 0 pin(s)
nixheal: pinned host laptop: nixpkgs.sbctl at 718f670e8453: /nix/store/...-sbctl-0.18.drv
nixheal: round 2: evaluating and building host laptop with 1 pin(s)
nixheal: host laptop builds
nixheal: committed "nixheal: update flake.lock"
> Building NixOS configuration
...
$ nixheal switch --update          # a few days later, sbctl is fixed upstream
nixheal: retrying host laptop: nixpkgs.sbctl: nixpkgs moved since it was pinned
nixheal: unpinned host laptop: nixpkgs.sbctl: builds again
```

## Quick start

nixheal is a flake. Add it, import the module on your host and point it at the
`pins.json` it will maintain next to `flake.lock`:

```nix
# flake.nix
inputs.nixheal = {
  url = "github:Sejder/nixheal";
  inputs.nixpkgs.follows = "nixpkgs";
};

# your host's configuration (with inputs passed in specialArgs)
{ inputs, ... }:
{
  imports = [ inputs.nixheal.nixosModules.default ];

  programs.nixheal = {
    enable = true;
    pinsFile = ./pins.json;          # path to it in your flake; nixheal creates the file
    flake = "/home/alice/nixos";     # the checkout nixheal updates
    hosts = [ "laptop" ];            # nixosConfigurations nixheal builds when it runs here
  };
}
```

Switch once so the module is active, then update through nixheal instead of
`nh os switch --update` or `nixos-rebuild switch --upgrade`:

```
nixheal switch --update
```

That updates the flake inputs, builds `nixosConfigurations.laptop` (pinning and
unpinning as needed), commits `flake.lock` and `pins.json`, and switches. Use
`nixheal boot --update` to activate on the next boot, or `nixheal build --update`
to only build and commit.

To have it run on its own, replace `system.autoUpgrade` with:

```nix
programs.nixheal.autoUpgrade = {
  enable = true;
  user = "alice";        # the owner of the checkout; activation runs as root
  operation = "switch";  # or "boot"
};
```

## Concepts

**Pin.** A top-level package (an attribute of `pkgs`, or of another package set
you register) taken from an older nixpkgs: the one in the last lock that
built. nixheal walks up the derivation graph from a failed build to the
packages your configuration uses directly and pins those, never a library on
its own, so nothing has to be rebuilt from source.

**Hold.** When a failure is not inside a pinnable package (configuration glue,
a denylisted core package such as the kernel, glibc, systemd, nix, stdenv or
gcc, or a package that fails at the old revision too), the host is held. On a
machine you switch by hand, nixheal then gives up instead: it restores
`flake.lock` and `pins.json`, commits nothing and switches nothing.

**Last good lock.** The lock nixheal last saw build, kept under
`$XDG_STATE_HOME/nixheal`; the current `flake.lock` otherwise.

**Retry.** Each pin records the input revision it was last tried at. When that
input moves, the pin is dropped for the next build; if the package builds, the
pin is gone, otherwise it comes back with its original date. Manual pins
(`nixheal pin`) are never retried.

## Commands

The verbs follow `nh` and `nixos-rebuild`:

| Command | |
|---|---|
| `nixheal build [--update [INPUT...]]` | pin and unpin until every configured host builds, then commit |
| `nixheal switch [--update [INPUT...]]` | the same, then switch this machine's host or home |
| `nixheal boot [--update [INPUT...]]` | the same, then make this machine boot into it |
| `nixheal status [--json]` | pins and held hosts with their age |
| `nixheal why <attr>` | the failed derivation, the path up to the pinned package, the end of the build log |
| `nixheal pin <attr> [--input I]` | pin by hand at the current lock; kept until cleared |
| `nixheal clear <attr>` / `--held` | remove a pin, or release a held host |
| `nixheal merge <file>` | fold another `pins.json` in; per host, the entry from the newer lock wins |

Options of `build`, `switch` and `boot`:

- `--update [INPUT...]` runs `nix flake update` first, for all inputs or the named ones.
- `--dry-run` shows what would be pinned and unpinned and changes nothing.
- `--no-commit` leaves `flake.lock` and `pins.json` uncommitted.
- `--only NAME` limits the run to some of the configured hosts and homes.
- `--hold-local` holds this machine's own host like any other instead of giving up; for a machine that builds a whole fleet.
- `--gc-roots DIR` keeps every system that built alive under `DIR/<host>` and `DIR/home-<name>`, e.g. for a binary cache.
- `--built-link PATH` points a symlink at this machine's built system, and removes it when the build failed, so a separate step can activate it.
- `-q`, `--quiet` prints only pins, unpins, holds, warnings and errors.

nixheal works in a dirty tree and only ever commits `flake.lock` and
`pins.json`. On a terminal, builds are shown through nix-output-monitor.

## Options

`nixosModules.default` and `homeManagerModules.default` share these options
under `programs.nixheal`:

| Option | Default | |
|---|---|---|
| `enable` | `false` | apply this machine's pins; install nixheal when `hosts` or `homes` is set |
| `pinsFile` | `null` | the flake's `pins.json` (it may not exist yet) |
| `flake` | `programs.nh.flake` | the checkout nixheal updates |
| `hosts`, `homes` | `[ ]` | `nixosConfigurations` / `homeConfigurations` nixheal builds when it runs here |
| `name` | host name / user name | this machine's entry in `pins.json` |
| `switcher` | `"nh"` | `"nh"`, or `"native"`: `nixos-rebuild` for hosts, `home-manager` for homes |
| `warnAfterDays` | `14` | log a pin at warning level, and report it as stale, after this many days |
| `deny` | `[ ]` | attributes never to pin, on top of the built-in list; `cuda*` matches a prefix |
| `quiet` | `false` | always run as `--quiet` |
| `nixpkgs.<input>` | `{ }` | another nixpkgs to import with pins applied (see below) |
| `autoUpgrade.enable` | `false` | a timer that runs nixheal and activates the result |
| `autoUpgrade.dates` | `"04:40"` | systemd calendar expression |
| `autoUpgrade.randomizedDelaySec` | `"45min"` | |
| `autoUpgrade.inputs` | `[ ]` | inputs to update; all when empty |
| `autoUpgrade.commit` | `true` | commit after each run |
| `autoUpgrade.operation` | `"switch"` | NixOS only: `"switch"` or `"boot"` |
| `autoUpgrade.user` | `"root"` | NixOS only: who updates, builds and commits; use the checkout's owner |

The settings are written to `/etc/nixheal.json` (NixOS) or
`~/.config/nixheal.json` (Home Manager), so nixheal keeps working when the
updated flake no longer evaluates.

### More package sets

Packages from a second nixpkgs, say `nixpkgs-unstable` on a stable system, are
pinned too if nixheal imports that nixpkgs for you:

```nix
programs.nixheal.nixpkgs.nixpkgs-unstable = {
  source = inputs.nixpkgs-unstable;
  config.allowUnfree = true;
  argName = "unstable-pkgs";   # modules can then take `unstable-pkgs` as an argument
};
```

### Home Manager

Inside NixOS, the Home Manager module follows the host's settings. Standalone,
it uses its own `homes` entry, and `nixheal switch` runs `nh home switch` (or
`home-manager switch` with the native switcher). `autoUpgrade` there is a user
timer that runs `nixheal switch --update`.

## A fleet

For several machines, let one of them build all of them and publish the result,
the way a binary cache host would:

1. On the build host, list every machine in `hosts` and run
   `nixheal build --update --hold-local --no-commit --gc-roots DIR` on a timer.
   A host that cannot be fixed is held; the others still get the new lock.
2. Publish `flake.lock` and `pins.json` together (`pins.json` first). Each host
   entry carries the `lockSha256` it was computed for, so a consumer can refuse
   a mismatched pair.
3. On each machine, install both into its checkout before rebuilding. A machine
   whose entry is `held` should skip the update and stay on what it runs.
4. Fold the published `pins.json` back into your repository with
   `nixheal merge`, so manual runs and the fleet do not overwrite each other.

`nixheal status --json` on the build host feeds a dashboard (see below).

## Dashboards

`nixheal status --json` prints one object for a dashboard, e.g. a
homepage-dashboard `customapi` widget:

```json
{"status":"PINNED","pins":1,"held":0,"stale":0,"oldest":"laptop: open-webui, 3d","oldest_days":3,
 "list":"laptop: open-webui (3d)","items":[{"name":"laptop: open-webui","label":"3d, eval",...}],
 "warn_after_days":14,"generated":"2026-10-30"}
```

`status` is the most urgent of `STALE` (a pin or hold older than
`warnAfterDays`), `HELD`, `PINNED` and `OK`. `items` has one row per pin or held
host for a `dynamic-list` widget, and a single `OK` row when nothing is
pinned. Ages are computed when it runs, so regenerate it regularly.

## Limitations

- **A pin holds back fixes for that package**, security fixes included. That is
  the trade: everything else updates. `warnAfterDays` and `status` make old
  pins visible; check `nixheal why` and fix or `nixheal clear` them.
- **Only top-level attributes are pinned.** A broken package inside a nested set
  (`python3Packages.foo`) is handled by pinning the top-level package that uses
  it; nixheal finds that from the build graph or from the evaluation trace.
- **Activation is not checked.** nixheal knows what builds, not what starts:
  a data migration a new release needs (say PostgreSQL collations after a
  glibc update) still fails at switch time.
- **Configurations are expected at `nixosConfigurations.<host>` and
  `homeConfigurations.<name>`**, with nixheal's module imported in each.
- **Evaluation dominates the run time.** Each round evaluates every host once,
  one after another, and a dirty git tree gets no evaluation cache. With nh, the
  switch reuses the system nixheal built instead of evaluating again.

## Compared with

- **`system.autoUpgrade`** updates and switches, but one failing package stops
  the whole update until someone fixes it. `programs.nixheal.autoUpgrade` is
  the same timer with pinning in between.
- **Pinning by hand in an overlay** works, but you have to notice the failure,
  find the right revision, and remember to remove the pin later. nixheal does
  all three and records why in `pins.json`.

## pins.json

```json
{ "version": 2,
  "hosts": {
    "laptop": { "lockSha256": "...", "lockLastModified": 1793083946,
                "pins": { "nixpkgs": { "sbctl": { "locked": { ... }, "since": "2026-10-27", "reason": "build",
                                                  "failedDrv": "...", "trail": [ ... ], "checkedNarHash": "..." } } } },
    "server": { "held": { "since": "2026-10-27", "reason": "...", "lock": { ... } } } },
  "homes": { } }
```

`locked` is the flake.lock node of the nixpkgs the package is taken from, so
the overlay fetches it purely. Each run writes only the entries it built; when
two runs disagree, the entry computed from the newer lock wins.

## Development

```
cargo test             # unit tests
nix flake check        # package, overlay and module checks
tests/e2e.sh           # a toy nixpkgs that breaks and heals, end to end
```

`tests/e2e.sh` needs Nix with flakes and uses `target/debug/nixheal` (or
`$NIXHEAL`). nh, nixos-rebuild and home-manager are replaced by stubs.

## License

MIT, see [LICENSE](LICENSE).
