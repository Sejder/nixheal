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
nixheal: flake /home/alice/nixos (config /etc/nixheal.json)
nixheal: round 1: evaluating and building host laptop with 0 pinned package(s)
nixheal: pinned host laptop: sbctl (nixpkgs) at 718f670e8453 from 2026-10-20: sbctl-0.18 failed to build
nixheal: the errors above were handled: pinned 1 package(s), building again
nixheal: round 2: evaluating and building host laptop with 1 pinned package(s)
nixheal: host laptop builds
nixheal: done: 1 built, 1 pinned; flake.lock and pins.json changed but not committed (...)
nixheal: switching host laptop with nh
...
$ nixheal switch --update          # a few days later, sbctl is fixed upstream
nixheal: retrying host laptop: sbctl (nixpkgs): nixpkgs moved since it was pinned
nixheal: unpinned host laptop: sbctl (nixpkgs): builds again
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
    pinsFile = ../../pins.json;      # the pins.json at the root of your flake; nixheal creates it
    flake = "/home/alice/nixos";     # the checkout nixheal updates
    hosts = [ "laptop" ];            # nixosConfigurations nixheal builds when it runs here
  };
}
```

Three things have to line up, and nixheal checks them before it builds
anything:

- **`pinsFile` is the `pins.json` next to `flake.lock`**, written as a path
  (`./pins.json`, or `../../pins.json` from a module in `hosts/laptop/`). The
  flake may live in a subdirectory of its git repository.
- **`programs.nixheal.name` is the configuration's name**:
  `nixosConfigurations.laptop` needs `name = "laptop"`. It defaults to
  `networking.hostName` (Home Manager: `home.username`), so set it when those
  differ.
- **`programs.nixheal.input` is the flake input `pkgs` comes from**, as named in
  `flake.lock`. The default is `nixpkgs`; set it if yours is called `nixos`.

Switch once so the module is active, then update through nixheal instead of
`nh os switch --update` or `nixos-rebuild switch --upgrade`:

```
nixheal switch --update
```

That updates the flake inputs, builds `nixosConfigurations.laptop` (pinning and
unpinning as needed) and switches, leaving `flake.lock` and `pins.json` changed
in the checkout for you to commit. Pass `--commit`, or set
`programs.nixheal.commit = true`, to have nixheal commit exactly those two
files after each run. Use `nixheal boot --update` to activate on the next
boot, or `nixheal build --update` to only build.

To have it run on its own, replace `system.autoUpgrade` with:

```nix
programs.nixheal.autoUpgrade = {
  enable = true;
  user = "alice";        # the owner of the checkout; activation runs as root
  operation = "switch";  # or "boot"
};
```

The timer builds the checkout as it is, uncommitted changes included, so keep
work in progress on a branch or out of the tree when it runs. `user` runs the
update and the build, and root then activates what it built, so whoever can
write to the checkout or act as `user` can decide what root activates: treat
that account as root. `autoUpgrade.onFailure` names units to start when a run
fails, for a notification.

### Home Manager, standalone

```nix
# homeConfigurations.alice, with the Home Manager module imported
programs.nixheal = {
  enable = true;
  pinsFile = ./pins.json;
  flake = "/home/alice/nixos";
  homes = [ "alice" ];
};
```

`name` defaults to `home.username` and must be the `homeConfigurations` key, so
set it for keys such as `"alice@laptop"`. `nixheal switch --update` then runs
`nh home switch`.

## Requirements

- Nix 2.19 or newer with flakes; developed against Nix 2.34. Lix is untested.
- nh 4.x when `switcher = "nh"` (tested with 4.4.2). The module installs nh
  when `programs.nh` does not.
- git, for committing; without it nixheal still runs and says it cannot
  commit.

## Concepts

**Pin.** A top-level package (an attribute of `pkgs`, or of another package set
you register) taken from an older nixpkgs: the one in the last lock that
built. nixheal walks up the derivation graph from a failed build to the
packages your configuration uses directly and pins those, not the library
underneath. A derivation is matched to an attribute by its `pname`, its name
without the version, and versioned names such as `nodejs_22`, each confirmed
against the derivation path. It never pins a package that more than 25 other
packages in the system depend on (nested ones such as `python3Packages.*`
included): the pinned copy would replace it for all of them, and every one
would rebuild from source. Such a failure holds the host instead.

A package that several of your package sets provide as the same derivation
(say `cloudflared` from `nixpkgs` and `nixpkgs-unstable` at the same revision)
is one failure: it is pinned in every set that can take the pin, skipping a set
that did not move or is new in the lock, and reported, counted, retried and
released as one package. `status` shows the sets next to it.

**Hold.** When a failure is not inside a pinnable package (configuration glue,
a configuration that no longer evaluates, such as a removed NixOS option, a
denylisted core package such as the kernel, glibc, systemd, nix, stdenv or
gcc, a package too much depends on, a package that fails at the old revision
too, or a package nixpkgs refuses to evaluate because it became insecure,
unfree or unsupported), the host is held: nixheal records the lock it last
built with in its entry and does not build it on the new one. The shared
`flake.lock` still moves for the other hosts, so a held machine only stays on
its old system if whatever updates it checks `held` (see [A fleet](#a-fleet));
a run on the machine itself never activates a held host. A held host is
retried on every run and released once it builds; `nixheal clear --held`
releases it by hand. Packages marked broken are pinned like failed builds. The
machine
nixheal runs on is never held unless you pass `--hold-local`: nixheal gives up
on it instead, for `build`, `switch`, `boot` and the timer alike. It restores
`flake.lock` and `pins.json`, commits nothing and activates nothing.

**Infrastructure.** A build that cannot run on this machine at all (another
platform, no builder, a timeout, out of memory, a build killed by a signal) is
not a broken package: nothing is pinned for it, and the hosts that need it are
held (this machine's own is given up on, as above) while the others move on. A
build killed by a signal, or a failed download that is not a hash mismatch, is
retried once first. A build that fails without nix naming a derivation (the
daemon went away, the disk is full) says nothing about any host, so the run
stops and restores the files.

**Last good lock.** The lock a target last built with, kept per flake and
target under `$XDG_STATE_HOME/nixheal`. For a target that has not built yet,
the last lock any target of the flake built with, then the current
`flake.lock`.

**Retry.** Each pin records the input revision it was last tried at. When that
input moves, the pin (with the package's pins in other sets from the same
failure) is dropped for the next build; if the package builds, the pin is
gone, otherwise it comes back with its original date. Manual pins
(`nixheal pin`) are never retried.

## Commands

The verbs follow `nh` and `nixos-rebuild`:

| Command | |
|---|---|
| `nixheal build [--update [INPUT...]]` | pin and unpin until every configured host builds; commit with `--commit` |
| `nixheal switch [--update [INPUT...]]` | the same, then switch this machine's host or home |
| `nixheal boot [--update [INPUT...]]` | the same, then make this machine boot into it |
| `nixheal status [--json]` | pinned packages and held hosts with their age |
| `nixheal why <attr>` | the failed derivation (or the evaluation error), the path up to the pinned package, the end of the build log |
| `nixheal pin <attr> [--input I]` | pin by hand at the current lock (in `programs.nixheal.input` by default); kept until cleared |
| `nixheal clear <attr> [--input I]` (or `unpin`) | remove a pin; the next run builds the package from the current lock and pins it again if it still fails |
| `nixheal clear --held` | release a held host or home: drop its held lock, so machines that skip held entries update again, and the next run builds it with the current lock |
| `nixheal merge <file>` | fold another `pins.json` in; per host or home, the entry from the newer lock wins, keeping pins made by hand here |

`why`, `pin` and `clear` act on this machine's entry, or on `--host NAME` /
`--home NAME`, which must be configured (or, for `why` and `clear`, have an
entry). `pin`, `clear` and `merge` only change `pins.json`; the next `build`,
`switch` or `boot` applies it, and commits it with `--commit` or
`programs.nixheal.commit`.

`--config FILE`, `--flake DIR`, `-q` and `-v` (`--verbose`, which overrides
`programs.nixheal.quiet`) work before or after the command.
Every command but `status` and `why` says first which flake and config it
acts on, since by default both come from `/etc/nixheal.json` or
`~/.config/nixheal.json`, wherever you run it. A `--config` (or
`$NIXHEAL_CONFIG`) that cannot be read is an error; a directory without a
`flake.lock` is refused by every command that writes.

Options of `build`, `switch` and `boot`:

| Option | |
|---|---|
| `-u`, `--update [INPUT...]` | run `nix flake update` first, for all inputs or the named ones (an unknown name is an error) |
| `--dry-run` | evaluate and build as usual and show what would be pinned and unpinned (marked `(dry run)`), then leave every file as it was |
| `--commit` | commit `flake.lock` and `pins.json` after the run (the default with `programs.nixheal.commit = true`) |
| `--no-commit` | leave them uncommitted, whatever `programs.nixheal.commit` says (the default otherwise) |
| `--only NAME` | build only some of the configured hosts and homes; repeatable or comma-separated |
| `--hold-local` | hold this machine's own host like any other instead of giving up; for a machine that builds a fleet |
| `--gc-roots DIR` | keep every system that built alive under `DIR/nixheal-host-<host>` and `DIR/nixheal-home-<name>`, e.g. for a binary cache; without `--only`, the roots this flake's runs made for targets no longer configured are removed (they are listed in a hidden file there), and nothing else in `DIR` is touched |
| `--fail-on-hold` | exit with 3 when a host or home is held |
| `--built-link PATH` | a GC root pointing at this machine's built system, removed when it did not build, so a separate step can activate it |
| `--old-lock FILE` | the lock to pin back to, instead of the last good one |
| `--max-rounds N` | give up after N build rounds (8) |
| `--max-new-pins N` | hold a host, or give up on this machine's own, rather than pin more than N new packages for it in one run (10) |
| `--system SYSTEM` | the system for eval checks when a target's own cannot be evaluated |
| `-q`, `--quiet` | print only pins, unpins, holds, warnings and errors; nix's error messages are shown only when nixheal could not handle them |

nixheal works in a dirty tree. When it commits, it commits only `flake.lock`
and `pins.json`, without running git hooks or signing. A run takes a lock on the
flake (also taken by `pin`, `clear` and `merge`), so a timer run and a manual
run cannot overlap, and Ctrl-C or SIGTERM puts both files back. A flake path
containing `#` or `?` is refused, since nix would read those as part of a
flake reference. On a terminal, builds are shown through
nix-output-monitor.

Every configured host and home must exist in the flake; a name that does not
is an error before anything builds.

Exit codes: 0 when everything built or was held, 1 when nixheal gave up on this
machine's own host, 2 on errors, 3 with `--fail-on-hold` when something is
held, 130 when interrupted. After a successful build,
`switch` and `boot` return the exit code of the switcher.

The settings come from `$NIXHEAL_CONFIG`, then `~/.config/nixheal.json`
(Home Manager), then `/etc/nixheal.json` (NixOS), whichever exists first. The
modules write them; by hand, the file looks like this (`kind` and `self` are
required, the rest default as the module options do):

```json
{ "kind": "nixos", "self": "laptop", "flake": "/home/alice/nixos",
  "hosts": ["laptop"], "homes": [], "input": "nixpkgs", "warnAfterDays": 14,
  "deny": [], "quiet": false, "commit": false, "switcher": "nh" }
```

## Options

`nixosModules.default` and `homeManagerModules.default` (also exported as
`homeModules.default`) share these options under `programs.nixheal`:

| Option | Default | |
|---|---|---|
| `enable` | `false` | apply this machine's pins; install nixheal when `hosts` or `homes` is set |
| `pinsFile` | `null` | the `pins.json` next to `flake.lock`, as a path (it may not exist yet) |
| `name` | host name / user name | this machine's entry in `pins.json`; must be its configuration's name |
| `input` | `"nixpkgs"` | the flake input `pkgs` comes from |
| `flake` | `programs.nh.flake` | the checkout nixheal updates (not a store path) |
| `hosts`, `homes` | `[ ]` | `nixosConfigurations` / `homeConfigurations` nixheal builds when it runs here |
| `switcher` | `"nh"` | `"nh"` (4.x, given the built system's store path), or `"native"`: `nixos-rebuild` (with `--sudo`, i.e. nixos-rebuild-ng) for hosts, `home-manager` for homes |
| `warnAfterDays` | `14` | report a pin or hold as stale, and log it as a warning, after this many days |
| `deny` | `[ ]` | attributes never to pin, on top of the built-in list; `cuda*` matches a prefix |
| `quiet` | `false` | always run as `--quiet` |
| `commit` | `false` | commit `flake.lock` and `pins.json` after each run, as with `--commit` |
| `package` | this flake's | the nixheal package |
| `nixpkgs.<input>` | `{ }` | another nixpkgs to import with pins applied: `source`, `config` (default: this host's `nixpkgs.config`), `argName` |
| `packageSets` | read-only | every package set nixheal pins on this machine, keyed by input |
| `autoUpgrade.enable` | `false` | a timer that runs nixheal and activates the result |
| `autoUpgrade.dates` | `"04:40"` | systemd calendar expression |
| `autoUpgrade.randomizedDelaySec` | `"45min"` | |
| `autoUpgrade.inputs` | `[ ]` | inputs to update; all when empty |
| `autoUpgrade.commit` | `null` | commit after each timer run; `null` follows `commit` |
| `autoUpgrade.operation` | `"switch"` | NixOS only: `"switch"` or `"boot"` |
| `autoUpgrade.user` | (required) | NixOS only: the owner of the checkout, who updates, builds and commits |
| `autoUpgrade.onFailure` | `[ ]` | units to start when a run fails |
| `autoUpgrade.timeout` | `"12h"` | stop a run that takes longer, e.g. one stuck on a hung download |

The settings are written to `/etc/nixheal.json` (NixOS) or
`~/.config/nixheal.json` (Home Manager), so nixheal keeps working when the
updated flake no longer evaluates.

The pin overlay is added with `mkAfter` and replaces each pinned attribute with
the old nixpkgs' copy, so your own overlays do not apply to a package while it
is pinned. `lib.mkOverlay { pins, kind ? "hosts", name, input, nixpkgsConfig ? null }`
is the overlay itself, for package sets the modules do not import. Sets made
that way are not in `packageSets`, so nixheal does not match failures to them;
use `programs.nixheal.nixpkgs` for that.

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

`argName` cannot be a name modules already get, such as `pkgs` or `lib`.

### Home Manager

Inside NixOS, the Home Manager module follows the host's settings and applies
its pins; `hosts`, `homes` and `autoUpgrade` set there are ignored with a
warning, so set them in the NixOS configuration. Standalone, it uses its own
`homes` entry, and `nixheal switch` runs `nh home switch` (or
`home-manager switch` with the native switcher). `autoUpgrade` there is a user
timer that runs `nixheal switch --update`; it runs while you are logged in, or
always with `loginctl enable-linger`.

## A fleet

For several machines, let one of them build all of them and publish the result,
the way a binary cache host would:

1. On the build host, list every machine in `hosts` and run
   `nixheal build --update --hold-local --no-commit --gc-roots DIR` on a timer.
   A host that cannot be fixed is held; the others still get the new lock.
2. Publish `flake.lock` and `pins.json` together (`pins.json` first). Each host
   entry carries the `lockSha256` it was computed for, so a consumer can refuse
   a mismatched pair. A held entry keeps the `lockSha256` of the lock it is held
   on, so check `held` before comparing.
3. On each machine, install both into its checkout before rebuilding. A machine
   whose entry is `held` should skip the update and stay on what it runs.
4. Fold the published `pins.json` back into your repository with
   `nixheal merge`, so manual runs and the fleet do not overwrite each other.

`nixheal status --json` on the build host feeds a dashboard (see below).

## Dashboards

`nixheal status --json` prints one object for a dashboard, e.g. a
homepage-dashboard `customapi` widget:

```json
{"status":"PINNED","pins":1,"pin_entries":2,"held":0,"stale":0,
 "oldest":"host laptop: open-webui, 3d","oldest_days":3,"list":"host laptop: open-webui (3d)",
 "items":[{"name":"host laptop: open-webui","label":"3d, eval","inputs":["nixpkgs","nixpkgs-unstable"],...}],
 "warn_after_days":14,"generated":"2026-10-30"}
```

`status` is the most urgent of `STALE` (a pin or hold older than
`warnAfterDays`), `HELD`, `PINNED` and `OK`. Manual pins never go stale. `items`
has one row per pinned package or held host for a `dynamic-list` widget, and a
single `OK` row when nothing is pinned; targets are named `host NAME` and
`home NAME`, as everywhere else. `pins` counts packages, `pin_entries` the pins
across package sets. For scripts, each row also has `target`, `target_kind`
(`host` or `home`) and `since`, plus `inputs` and `attr` for a pin or
`held_reason` for a hold. Ages are in local days
and computed when it runs, so regenerate it regularly. The text `status` marks
the same pins and holds `STALE`.

The text `nixheal status`:

```
flake /home/alice/nixos
TARGET       PACKAGE (SETS)                         REASON           SINCE
host laptop  open-webui (nixpkgs, nixpkgs-unstable)  failed to build  2026-10-20 (8d)
host server  -                                       whole host held  2026-10-01 (27d) STALE
    nixpkgs.linux fails and is on the never-pin list (...)
1 pinned package(s), 1 held, 1 stale (older than 14 days).
'nixheal why ATTR' explains a pin, 'nixheal clear ATTR' removes one, 'nixheal clear --held --host NAME' releases a hold.
```

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
- **Home Manager inside NixOS with `home-manager.useGlobalPkgs = false`** has
  its own `pkgs`, but its failures are matched against the host's package
  sets; a package that differs between the two may hold the host instead of
  being pinned. Use `useGlobalPkgs = true`, or a standalone Home Manager.
- **Linux only.** The package is built for x86_64-linux and aarch64-linux.
- **A read-only nixpkgs** (`nixosModules.readOnlyPkgs`) cannot take the
  module's overlay; the module warns, and you add `lib.mkOverlay` to the `pkgs`
  you pass in yourself.
- **Nix's log format.** nixheal reads the messages of current Nix (the
  "Cannot build ... Reason:" format) and the older "builder for ... failed"
  one; after a failed build it also checks that each system it counts as
  built really is in the store.
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
the overlay fetches it purely. A run replaces the entries of the targets it
built and leaves the others alone. `nixheal merge` combines two files: per
target, the entry computed from the newer lock wins.

## Development

```
cargo test             # unit tests
nix flake check        # package, overlay, module and activation checks
tests/e2e.sh           # a toy nixpkgs that breaks and heals, end to end
```

`tests/e2e.sh` needs Nix with flakes and uses `target/debug/nixheal` (or
`$NIXHEAL`). nh, nixos-rebuild and home-manager are replaced by stubs.

## License

MIT, see [LICENSE](LICENSE).
