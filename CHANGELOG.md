# Changelog

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
