#!/usr/bin/env bash
# End-to-end run of autopin against a toy nixpkgs whose history breaks and then
# fixes packages. Needs a working nix with flakes; run from anywhere:
#
#   cargo build && tests/e2e.sh
#
# AUTOPIN overrides the binary under test.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
autopin="${AUTOPIN:-$repo/target/debug/autopin}"
system="$(nix eval --raw --impure --expr builtins.currentSystem)"
nix() { command nix --extra-experimental-features "nix-command flakes" "$@"; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
toy="$work/toypkgs"
sys="$work/system"
log="$work/log"
salt="$(date +%s%N)"

step() { printf '\n=== %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  [ -f "$log" ] && sed 's/^/  | /' "$log" >&2
  exit 1
}

pins() {
  nix eval --raw --impure --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/failures.json); in
    builtins.concatStringsSep \" \" (builtins.concatLists (builtins.attrValues
      (builtins.mapAttrs (i: s: map (a: i + \".\" + a) (builtins.attrNames s)) (f.pins or { }))))"
}

field() {
  nix eval --raw --impure --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/failures.json); in
    toString (f.pins.$1.$2.$3 or \"\")"
}

expect_pins() {
  local got
  got="$(pins)"
  [ "$got" = "$1" ] || fail "expected pins [$1], got [$got]"
  echo "pins: [${got}]"
}

expect_log() {
  grep -qF -- "$1" "$log" || fail "log lacks: $1"
}

reject_log() {
  if grep -qF -- "$1" "$log"; then fail "log unexpectedly has: $1"; fi
}

expect_built() {
  local out
  out="$(nix build --no-link --print-out-paths "$sys#toplevel")"
  grep -qF -- "$1" "$out" || fail "toplevel does not contain $1: $(cat "$out")"
}

revision() {
  local libfoo="$1" gone="$2" hello="$3"
  cat >"$toy/state.nix" <<EOF
{ libfoo = "$libfoo"; goneRemoved = $gone; hello = "$hello"; salt = "$salt"; }
EOF
  git -C "$toy" add -A
  git -C "$toy" -c user.name=e2e -c user.email=e2e@localhost commit -qm "libfoo=$libfoo gone-removed=$gone hello=$hello"
}

publish() { cp "$sys/flake.lock" "$work/published.lock"; }

run_autopin() {
  set +e
  "$autopin" run --flake "$sys" --old-lock "$work/published.lock" \
    --output machine=toplevel --pkgs toypkgs=pkgs --system "$system" "$@" 2>"$log"
  local rc=$?
  set -e
  sed 's/^/  | /' "$log" | grep 'autopin:' || true
  return $rc
}

[ -x "$autopin" ] || fail "$autopin is not built; run cargo build first"

mkdir -p "$toy" "$sys"
git -C "$toy" init -q -b main

cat >"$toy/default.nix" <<'EOF'
{ system, config ? { }, overlays ? [ ] }:
let
  state = import ./state.nix;
  mk = name: pname: deps: script: derivation {
    inherit name pname system deps;
    inherit (state) salt;
    builder = "/bin/sh";
    args = [ "-c" script ];
  };
  base = self: {
    inherit config;
    stdenv.hostPlatform.system = system;
    libfoo = mk "libfoo-1.0" "libfoo" [ ] (
      if state.libfoo == "broken" then "echo libfoo does not compile >&2; exit 1"
      else "echo libfoo-${state.libfoo} > $out"
    );
    app = mk "app-2.0" "app" [ self.libfoo ] "echo app $deps > $out";
    hello = mk "hello-${state.hello}" "hello" [ ] "echo hello > $out";
    gone = if state.goneRemoved then throw "'gone' has been removed" else mk "gone-1.0" "gone" [ ] "echo gone > $out";
  };
  extend = f: o: self: let prev = f self; in prev // o self prev;
  fix = f: let x = f x; in x;
in
fix (builtins.foldl' extend base overlays)
EOF

cp "$repo/nix/overlay.nix" "$sys/overlay.nix"
cat >"$sys/flake.nix" <<EOF
{
  inputs.toypkgs = { url = "git+file://$toy"; flake = false; };
  outputs = { self, toypkgs }:
    let
      system = "$system";
      pkgs = import toypkgs {
        inherit system;
        overlays = [ (import ./overlay.nix { failures = ./failures.json; input = "toypkgs"; }) ];
      };
      systemPath = derivation {
        name = "system-path";
        inherit system;
        builder = "/bin/sh";
        args = [ "-c" "echo \$paths > \$out" ];
        paths = [ pkgs.app pkgs.hello pkgs.gone ];
      };
    in {
      inherit pkgs;
      toplevel = derivation {
        name = "toplevel";
        inherit system;
        builder = "/bin/sh";
        args = [ "-c" "read -r line < \$sp; echo \"\$line\" > \$out" ];
        sp = systemPath;
      };
    };
}
EOF

step "r1: everything builds"
revision ok false 1
nix flake lock "$sys"
expect_built hello-1
publish

step "r2: libfoo breaks (build) and gone is removed (eval)"
revision broken true 2
nix flake update --flake "$sys"
run_autopin || fail "autopin gave up on r2"
expect_pins "toypkgs.app toypkgs.gone"
[ "$(field toypkgs app reason)" = build ] || fail "app should be a build pin"
[ "$(field toypkgs gone reason)" = eval ] || fail "gone should be an eval pin"
case "$(field toypkgs app failedDrv)" in *-libfoo-1.0.drv) ;; *) fail "app should name libfoo as the failed drv" ;; esac
expect_log "pinned toypkgs.gone"
expect_log "pinned toypkgs.app"
expect_built hello-2
since_gone="$(field toypkgs gone since)"
publish

step "r2 again: the input did not move, so nothing is retried"
run_autopin || fail "autopin gave up on the r2 rerun"
reject_log "retrying"
expect_pins "toypkgs.app toypkgs.gone"

step "r3: libfoo is fixed, gone is still removed"
revision fixed true 3
nix flake update --flake "$sys"
run_autopin || fail "autopin gave up on r3"
expect_log "retrying toypkgs.app"
expect_log "unpinned toypkgs.app: builds again"
expect_log "still failing toypkgs.gone"
expect_pins "toypkgs.gone"
[ "$(field toypkgs gone since)" = "$since_gone" ] || fail "gone lost its original date"
expect_built hello-3
publish

step "r4: gone is back, every pin clears"
revision fixed false 4
nix flake update --flake "$sys"
run_autopin || fail "autopin gave up on r4"
expect_log "unpinned toypkgs.gone: builds again"
expect_pins ""
expect_built hello-4
publish

step "r5: libfoo breaks again but app is denylisted, so autopin gives up"
revision broken false 5
nix flake update --flake "$sys"
cp "$sys/failures.json" "$work/before.json"
if run_autopin --deny app; then fail "autopin should have given up on r5"; fi
expect_log "giving up: toypkgs.app fails but is never pinned"
cmp -s "$sys/failures.json" "$work/before.json" || fail "failures.json changed on give-up"

printf '\nall e2e steps passed\n'
