#!/usr/bin/env bash
# End-to-end run of autopin against a toy nixpkgs whose history breaks and then
# fixes packages, on two toy hosts: a (the machine autopin runs on) and b.
# Needs a working nix with flakes; run from anywhere:
#
#   cargo build && tests/e2e.sh
#
# AUTOPIN overrides the binary under test. nh is replaced by a stub that
# records its arguments.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
autopin="${AUTOPIN:-$repo/target/debug/autopin}"
nix() { command nix --extra-experimental-features "nix-command flakes" "$@"; }
system="$(nix eval --raw --impure --expr builtins.currentSystem)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
toy="$work/toypkgs"
sys="$work/system"
log="$work/log"
calls="$work/nh.calls"
config="$work/autopin.json"
salt="$(date +%s%N)"

export XDG_STATE_HOME="$work/state"
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@localhost
export GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@localhost
mkdir -p "$work/bin"
printf '#!/bin/sh\necho "$*" >> %s\n' "$calls" >"$work/bin/nh"
chmod +x "$work/bin/nh"
export PATH="$work/bin:$PATH"
touch "$calls"

step() { printf '\n=== %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  [ -f "$log" ] && sed 's/^/  | /' "$log" >&2
  exit 1
}

jq_pins() {
  nix eval --raw --impure --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/pins.json); in $1"
}

pins_of() {
  jq_pins "builtins.concatStringsSep \" \" (builtins.concatLists (builtins.attrValues
    (builtins.mapAttrs (i: s: map (a: i + \".\" + a) (builtins.attrNames s)) ((f.hosts.$1 or { }).pins or { }))))"
}

field() { jq_pins "toString (f.hosts.$1.pins.$2.$3.$4 or \"\")"; }
held_reason() { jq_pins "(f.hosts.$1.held or { }).reason or \"\""; }

expect_pins() {
  local got
  got="$(pins_of "$1")"
  [ "$got" = "$2" ] || fail "expected pins on $1 [$2], got [$got]"
  echo "pins on $1: [$got]"
}

expect_log() { grep -qF -- "$1" "$log" || fail "log lacks: $1"; }
reject_log() { if grep -qF -- "$1" "$log"; then fail "log unexpectedly has: $1"; fi; }
commits() { git -C "$sys" rev-list --count HEAD; }
nh_calls() { wc -l <"$calls" | tr -d ' '; }

expect_built() {
  local out
  out="$(nix build --no-link --print-out-paths "$sys#nixosConfigurations.$1.config.system.build.toplevel")"
  grep -qF -- "$2" "$out" || fail "$1 does not contain $2: $(head -c 300 "$out")"
}

write_config() {
  cat >"$config" <<EOF
{ "kind": "nixos", "self": "a", "flake": "$sys", "hosts": ["a", "b"], "homes": [],
  "inputs": ["toypkgs"], "warnAfterDays": 14, "deny": [$1] }
EOF
}

autopin_() {
  set +e
  "$autopin" "$@" --config "$config" >"$work/stdout" 2>"$log"
  local rc=$?
  set -e
  cat "$work/stdout" >>"$log"
  grep -h 'autopin:' "$log" | sed 's/^/  | /' || true
  return $rc
}

revision() {
  cat >"$toy/state.nix" <<EOF
{ libfoo = "$1"; goneRemoved = $2; hello = "$3"; linux = "$4"; salt = "$salt"; }
EOF
  git -C "$toy" add -A
  git -C "$toy" commit -qm "libfoo=$1 gone-removed=$2 hello=$3 linux=$4"
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
  broken = what: "echo ${what} does not compile >&2; exit 1";
  base = self: {
    inherit config;
    stdenv.hostPlatform.system = system;
    libfoo = mk "libfoo-1.0" "libfoo" [ ] (
      if state.libfoo == "broken" then broken "libfoo" else "echo libfoo-${state.libfoo} > $out"
    );
    app = mk "app-2.0" "app" [ self.libfoo ] "echo app $deps > $out";
    hello = mk "hello-${state.hello}" "hello" [ ] (
      if state.hello == "broken" then broken "hello" else "echo hello > $out"
    );
    linux = mk "linux-6.0" "linux" [ ] (
      if state.linux == "broken" then broken "linux" else "echo linux-${state.linux} > $out"
    );
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
      mkHost = name: paths:
        let
          pkgs = import toypkgs {
            inherit system;
            overlays = [ (import ./overlay.nix { pins = ./pins.json; inherit name; input = "toypkgs"; }) ];
          };
          sp = derivation {
            name = "system-path-\${name}";
            inherit system;
            builder = "/bin/sh";
            args = [ "-c" "echo \$paths > \$out" ];
            paths = paths pkgs;
          };
        in {
          config.programs.autopin.packageSets.toypkgs = pkgs;
          config.system.build.toplevel = derivation {
            name = "toplevel-\${name}";
            inherit system;
            builder = "/bin/sh";
            args = [ "-c" "read -r line < \$sp; echo \"\$line\" > \$out" ];
            inherit sp;
          };
        };
    in {
      nixosConfigurations.a = mkHost "a" (p: [ p.hello p.gone ]);
      nixosConfigurations.b = mkHost "b" (p: [ p.app p.hello p.linux ]);
    };
}
EOF
write_config ""

git -C "$sys" init -q -b main
git -C "$sys" add flake.nix overlay.nix
revision ok false 1 ok
nix flake lock "$sys"
git -C "$sys" add flake.lock
git -C "$sys" commit -qm system

step "r1: everything builds; update creates pins.json, commits it and switches a"
autopin_ update || fail "autopin update gave up on r1"
expect_log "created $sys/pins.json"
git -C "$sys" ls-files --error-unmatch pins.json >/dev/null || fail "pins.json is not tracked"
git -C "$sys" diff --quiet HEAD -- pins.json || fail "pins.json is not committed"
[ "$(tail -n1 "$calls")" = "os switch $sys -H a" ] || fail "nh not called for a: $(cat "$calls")"
expect_pins a ""
expect_pins b ""

step "r2: libfoo breaks (only b uses app) and gone is removed (only a uses it)"
revision broken true 2 ok
before_calls="$(nh_calls)"
autopin_ update || fail "autopin update gave up on r2"
expect_pins a "toypkgs.gone"
expect_pins b "toypkgs.app"
[ "$(field a toypkgs gone reason)" = eval ] || fail "gone on a should be an eval pin"
[ "$(field b toypkgs app reason)" = build ] || fail "app on b should be a build pin"
git -C "$sys" log -1 --format=%B | grep -qF "pinned host b: toypkgs.app" || fail "commit message lacks the b pin"
git -C "$sys" log -1 --format=%B | grep -qF "pinned host a: toypkgs.gone" || fail "commit message lacks the a pin"
[ "$(nh_calls)" -gt "$before_calls" ] || fail "nh was not called on r2"
expect_built a hello-2
expect_built b hello-2
since_gone="$(field a toypkgs gone since)"

step "why: explain the app pin on b"
autopin_ why app --host b || fail "autopin why failed"
expect_log "-libfoo-1.0.drv"
expect_log "trail:    libfoo-1.0 -> app-2.0"
expect_log "libfoo does not compile"

step "r2 switch: no update, so nothing is retried and nothing is committed"
before_commits="$(commits)"
autopin_ switch || fail "autopin switch gave up on r2"
reject_log "retrying"
[ "$(commits)" = "$before_commits" ] || fail "switch committed without changes"
[ "$(tail -n1 "$calls")" = "os switch $sys -H a" ] || fail "switch did not call nh"

step "r3: libfoo is fixed, gone is still removed"
revision fixed true 3 ok
autopin_ update || fail "autopin update gave up on r3"
expect_log "unpinned host b: toypkgs.app: builds again"
expect_log "still failing host a: toypkgs.gone"
expect_pins a "toypkgs.gone"
expect_pins b ""
[ "$(field a toypkgs gone since)" = "$since_gone" ] || fail "gone lost its original date"
r3_lock="$work/r3.lock"
cp "$sys/flake.lock" "$r3_lock"

step "r4: linux breaks on b (denylisted): b is held on the r3 lock, a moves on"
revision fixed true 4 broken
autopin_ update || fail "autopin update gave up on r4"
expect_log "held host b on its last good lock"
case "$(held_reason b)" in *"linux fails but is never pinned"*) ;; *) fail "b should be held for linux: $(held_reason b)" ;; esac
held_rev="$(jq_pins 'f.hosts.b.held.lock.nodes.toypkgs.locked.rev')"
r3_rev="$(nix eval --raw --impure --expr "(builtins.fromJSON (builtins.readFile $r3_lock)).nodes.toypkgs.locked.rev")"
[ "$held_rev" = "$r3_rev" ] || fail "b is held on $held_rev, expected the r3 lock $r3_rev"
expect_built a hello-4
autopin_ status || fail "autopin status failed"
expect_log "host b	HELD since"

step "r5: linux is fixed and gone is back: b is released, a unpins"
revision fixed false 5 ok
autopin_ update || fail "autopin update gave up on r5"
expect_log "released host b"
expect_log "unpinned host a: toypkgs.gone: builds again"
expect_pins a ""
expect_pins b ""
[ -z "$(held_reason b)" ] || fail "b is still held"

step "r5 quiet switch: nothing changes, so nothing is printed"
autopin_ switch --quiet || fail "the quiet switch gave up"
reject_log "round 1:"
reject_log "builds"
[ -z "$(grep 'autopin:' "$log" || true)" ] || fail "the quiet switch printed: $(cat "$log")"

step "r6: hello breaks on a, the local host, and is denylisted: nothing changes"
write_config '"hello"'
revision fixed false broken ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
before_commits="$(commits)"
before_calls="$(nh_calls)"
if autopin_ update; then fail "autopin update should have given up on r6"; fi
expect_log "giving up on host a"
expect_log "restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored"
[ "$(commits)" = "$before_commits" ] || fail "a failed update committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "a failed update switched"
write_config ""

step "r6 dry run, quiet: shows the hello pins and nothing else, writes nothing"
autopin_ -q update --dry-run || fail "the dry run gave up"
reject_log "round 1:"
expect_log "dry run: would have pinned host a: toypkgs.hello"
expect_log "dry run: would have pinned host b: toypkgs.hello"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "the dry run changed flake.lock"
cmp -s "$sys/pins.json" "$work/before.json" || fail "the dry run changed pins.json"
[ "$(commits)" = "$before_commits" ] || fail "the dry run committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "the dry run switched"

step "manual pin: kept by switch, removed by clear"
autopin_ pin hello --input toypkgs || fail "autopin pin failed"
expect_pins a "toypkgs.hello"
[ "$(field a toypkgs hello reason)" = manual ] || fail "hello should be a manual pin"
autopin_ switch || fail "autopin switch gave up with a manual pin"
reject_log "retrying"
expect_pins a "toypkgs.hello"
git -C "$sys" log -1 --format=%s | grep -qF "autopin: update pins" || fail "switch did not commit the manual pin"
autopin_ clear hello || fail "autopin clear failed"
expect_log "cleared host a: toypkgs.hello"
expect_pins a ""

printf '\nall e2e steps passed\n'
