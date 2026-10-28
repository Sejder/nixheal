# Changelog

## Unreleased

Fixes from the second and third independent reviews of 0.1.1.

Failures:

- A configuration that no longer evaluates, such as one using a removed NixOS
  option, holds that host instead of stopping the run.
- A package nixpkgs refuses to evaluate as insecure, unfree or unsupported
  holds the host instead of being pinned around; a package marked broken is
  still pinned.
- A build that fails without naming a derivation fails the hosts that built,
  instead of none of them.
- A build killed by a signal, or a failed download that is not a hash
  mismatch, is retried once, and a signal counts as infrastructure rather than
  a broken package.
- Derivations are matched to versioned attributes such as `nodejs_22`.
- Derivation paths are recognised in any store directory.

Setups:

- A flake in a subdirectory of its git repository works; `pinsFile` is the
  `pins.json` next to `flake.lock`, and the run lock lives in the git directory.
- Home Manager inside NixOS takes its entry name from the host's
  `programs.nixheal.name` and its input from the host's
  `programs.nixheal.input`.
- Home Manager with `nixpkgs.config = null`, and NixOS with `nixpkgs.pkgs`
  set, no longer break the overlay.
- Extra package sets and pinned packages are imported for the right platforms
  when cross-compiling.
- The timers set `NIXHEAL_CONFIG`, so they read the settings of the module
  that defined them. The Home Manager timer has git and ssh on its PATH and
  requires `flake`.
- New `autoUpgrade.onFailure`, units to start when a run fails.
- nh is installed when it is the switcher and `programs.nh` is off.
- `nixheal pin` pins in `programs.nixheal.input` unless `--input` says
  otherwise.

Robustness:

- `pin`, `clear` and `merge` take the run lock and check the flake path like
  the build commands.
- A signal can no longer land between writing `pins.json` and restoring it, or
  between the commit and the end of guarding, and an error after the build
  (GC roots, the commit) restores both files.
- Commits run with `core.hooksPath=/dev/null`, so no hook can run.
- A flake path containing `#` or `?` is refused.
- `--gc-roots` removes the roots of targets no longer configured.
- `--quiet` keeps nix's own error messages.
- Journal output is detected by the inode in `JOURNAL_STREAM`, so a run piped
  from a service still prints plainly.

Pins, holds and roots:

- `--gc-roots` only removes roots it made: they are named
  `nixheal-host-NAME` and `nixheal-home-NAME`, so a host and a home can no
  longer share one, and other links in the directory are left alone. Roots
  under the old names are not removed; delete them once by hand.
- The Home Manager timer is kept running through the switch it starts
  (`X-SwitchMethod = keep-old`), and the NixOS one is not stopped when an
  activation drops it (`X-StopOnRemoval = false`).
- The cap on dependents counts every package above the one to pin, not only
  top-level attributes, so a library many `python3Packages` use is not pinned.
- After a failed build, a system counts as built only if it is in the store,
  and older Nix's "N dependencies of derivation ... failed to build", timeout
  and signal messages are understood.
- A host or home that is not in the flake is an error before anything builds,
  instead of being held.
- A build that fails without naming a derivation stops the run instead of
  holding every other host.
- A system that built in an early round is evaluated again with the final
  `pins.json`, and built again if that changed it.
- The evaluation trace is read in full when a failure inside a nested package
  needs the package that uses it.
- `merge` refuses a missing file and one that is not a pins file; every
  command that writes refuses a directory without `flake.lock`.

Command line:

- Every command that writes says which flake and config it acts on. An
  explicit `--config` or `$NIXHEAL_CONFIG` that cannot be read is an error.
- `--config`, `--flake` and `-q` work before or after the command, under their
  own help heading; every flag and argument has help text, and help wraps.
- `--update` with an input the flake does not have is an error.
- `pin`, `clear` and `clear --held` say what happens next and how to undo
  them; `pin` says when it replaces an existing pin, and `clear --held` no
  longer takes an attribute.
- Holds and give-ups say what to do; a host held before is "still" held.
- Dry runs mark their changes `(dry run)` instead of reporting them twice.
- A run ends with one line of what it built, pinned, unpinned and held, and
  what it committed. Short rounds and fresh pins are reported more quietly.
- `--quiet` holds back nix's error messages unless nixheal could not handle
  them, and without it handled errors are followed by a line saying so.
- `status` prints an aligned table with a header and a summary, names the
  flake, and says when there is no `pins.json`. `status --json` rows carry
  `target`, `target_kind`, `input`, `attr`, `since` and `held_reason`.
- `why` says where else a package is pinned and how to clear it.
- Error messages name where a path came from, cut nix's errors to the line
  that says what failed, and avoid store paths where a name will do.
- `--max-rounds 0` is refused.

## 0.1.1

Fixes from an independent review of 0.1.0.

Setups where pins silently never applied are now caught before anything
builds, with an error saying what to change:

- `programs.nixheal.name` must be the configuration's name: a host whose name
  differs (for example `networking.hostName = "alice-laptop"` on
  `nixosConfigurations.laptop`, or `homeConfigurations."alice@laptop"`) wrote
  its pins under one name and read them under another.
- `pinsFile` must be the `pins.json` at the flake root, where nixheal writes
  it, and a path rather than a string.
- `programs.nixheal.enable` must be on in every target.
- New `programs.nixheal.input`, for a main nixpkgs input not called `nixpkgs`.

Bugs:

- Fixed-output hash mismatches are recognised as build failures, so a broken download
  is pinned instead of failing every host in the round. Only hosts whose own
  build failed are affected by an unrecognised failure.
- Builds that cannot run here (another platform, no builder, a timeout, out of
  memory) stop the run without pinning or holding anything.
- `switch` and `boot` with `--hold-local` no longer activate a held local host.
- Each target uses its own package sets, read from its configuration, instead
  of the running machine's.
- The last good lock is kept per target, and saved only after the commit.
- A run writes the entries it built, instead of keeping a stale entry when the
  lock moved backwards.
- A run takes a lock on the flake, and Ctrl-C or SIGTERM restores `flake.lock`
  and `pins.json`.
- `--dry-run` leaves `--built-link` and a fresh checkout alone.
- `boot` on a Home Manager configuration is refused before building.
- `--only` with an unknown name is an error.
- `nixheal pin` checks that the package exists.
- Host names are quoted in attribute paths.
- Packages more than 25 others depend on are no longer pinned, since that
  rebuilt all of them from source; the host is held instead.

Safety:

- `autoUpgrade.user` is required, and nixheal refuses to run as root in a
  checkout another user owns, which left root-owned files there. Files are
  written through exclusively created temporary files that keep the original
  owner and mode, so a planted symlink is never followed.
- Commits skip git hooks and signing, so a hook or a GPG prompt cannot stall
  the timer. When git refuses a repository, nixheal says so.
- The root activation step only activates a NixOS system in the store, and
  `--built-link` is a GC root.
- The NixOS timer has ssh available for `git+ssh` inputs; the Home Manager
  timer drops a dependency the user manager cannot satisfy.
- `argName` cannot shadow a module argument such as `pkgs`.
- Home Manager inside NixOS warns when `hosts`, `homes` or `autoUpgrade` are
  set there, since they only apply standalone.
- A flake path inside the Nix store is refused.

Smaller changes:

- `homeModules.default` is exported next to `homeManagerModules.default`.
- `status` marks old pins and holds `STALE`, as the JSON does, and manual pins
  never go stale. Homes are named `home NAME` in the summary. Dates are local.
- The README documents every flag, the exit codes, the config lookup order, the
  three requirements above and the overlay's precedence.
- Tests cover the Home Manager module, the activation step, the new log
  formats, the probes, the dry run, holds without activation and interruption.
  CI lints test code too.

## 0.1.0

First public release.

- `nixheal build`, `switch` and `boot`, each with `--update [INPUT...]`:
  update the flake, build every configured host and home, pin packages that
  fail to the nixpkgs they last built with, unpin them once they build again,
  hold hosts pinning cannot fix, commit `flake.lock` and `pins.json`, and
  activate with nh, nixos-rebuild or home-manager.
- `nixheal status [--json]`, `why`, `pin`, `clear` and `merge`.
- NixOS and Home Manager modules (`programs.nixheal`) that apply the pins to
  `pkgs` and to further nixpkgs imports, write the settings to
  `/etc/nixheal.json` or `~/.config/nixheal.json`, and provide
  `autoUpgrade`, a timer in place of `system.autoUpgrade`.
- Fleet support: `--hold-local`, `--gc-roots` and `--built-link`, per-host
  entries in `pins.json` (format version 2) with the lock each was computed
  for, and `merge` with newer-lock-wins.
- `pins.json` format version 2 is stable from this release on.
