#!/usr/bin/env bash
# End-to-end run of nixheal against a toy nixpkgs whose history breaks and then
# fixes packages, on two toy hosts: a (the machine nixheal runs on) and b.
# Needs a working nix with flakes; run from anywhere:
#
#   cargo build && tests/e2e.sh
#
# NIXHEAL overrides the binary under test. nh, nixos-rebuild and home-manager
# are replaced by stubs that record their arguments.

set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
nixheal="${NIXHEAL:-$repo/target/debug/nixheal}"
nix() { command nix --extra-experimental-features "nix-command flakes" "$@"; }
system="$(nix eval --raw --impure --expr builtins.currentSystem)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
toy="$work/toypkgs"
sys="$work/system"
log="$work/log"
calls="$work/nh.calls"
config="$work/nixheal.json"
salt="$(date +%s%N)"

export XDG_STATE_HOME="$work/state"
export GIT_AUTHOR_NAME=e2e GIT_AUTHOR_EMAIL=e2e@localhost
export GIT_COMMITTER_NAME=e2e GIT_COMMITTER_EMAIL=e2e@localhost
mkdir -p "$work/bin"
for tool in nh nixos-rebuild home-manager; do
  printf '#!/bin/sh\necho "%s $*" >> %s\n' "$tool" "$calls" >"$work/bin/$tool"
  chmod +x "$work/bin/$tool"
done
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

# nh gets the system nixheal built, so it does not evaluate the flake again.
expect_switched_to_built() {
  local want
  want="$(nix eval --raw "$sys#nixosConfigurations.$1.config.system.build.toplevel.outPath")"
  [ "$(tail -n1 "$calls")" = "nh os switch $want" ] || fail "nh was not given the built $1 ($want): $(tail -n1 "$calls")"
}
nh_calls() { wc -l <"$calls" | tr -d ' '; }

expect_built() {
  local out
  out="$(nix build --no-link --print-out-paths "$sys#nixosConfigurations.$1.config.system.build.toplevel")"
  grep -qF -- "$2" "$out" || fail "$1 does not contain $2: $(head -c 300 "$out")"
}

write_config() {
  local hosts='"a", "b"'
  [ -n "${3:-}" ] && hosts="$3"
  cat >"$config" <<EOF
{ "kind": "nixos", "self": "a", "flake": "$sys", "hosts": [$hosts], "homes": [],
  "warnAfterDays": 14, "deny": [$1]${2:+, $2} }
EOF
}

write_home_config() {
  cat >"$config" <<EOF
{ "kind": "home", "self": "alice", "flake": "$sys", "hosts": [], "homes": ["alice"],
  "warnAfterDays": 14, "deny": [] }
EOF
}

nixheal_() {
  set +e
  "$nixheal" "$@" --config "$config" >"$work/stdout" 2>"$log"
  local rc=$?
  set -e
  cat "$work/stdout" >>"$log"
  grep -h 'nixheal:' "$log" | sed 's/^/  | /' || true
  return $rc
}

revision() {
  cat >"$toy/state.nix" <<EOF
{ libfoo = "$1"; goneRemoved = $2; hello = "$3"; linux = "$4"; salt = "$salt"; }
EOF
  git -C "$toy" add -A
  git -C "$toy" commit -qm "libfoo=$1 gone-removed=$2 hello=$3 linux=$4"
}

[ -x "$nixheal" ] || fail "$nixheal is not built; run cargo build first"

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
      if state.hello == "broken" then broken "hello"
      else if state.hello == "slow" then "i=0; while [ $i -lt 30000000 ]; do i=$((i + 1)); done; echo hello > $out"
      else "echo hello > $out"
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
      mkHost = name: paths: mkHostOf "hosts" name paths { };
      mkHostOf = kind: name: paths: settings:
        let
          pkgs = import toypkgs {
            inherit system;
            overlays = [ (import ./overlay.nix { pins = ./pins.json; inherit kind name; input = "toypkgs"; }) ];
          };
          sp = derivation {
            name = "system-path-\${name}";
            inherit system;
            builder = "/bin/sh";
            args = [ "-c" "echo \$paths > \$out" ];
            paths = paths pkgs;
          };
        in rec {
          config.programs.nixheal = {
            enable = true;
            inherit name;
            pinsFile = ./pins.json;
            packageSets.toypkgs = pkgs;
          } // settings;
          activationPackage = config.system.build.toplevel;
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
      homeConfigurations.alice = mkHostOf "homes" "alice" (p: [ p.hello ]) { };
      nixosConfigurations.misnamed = mkHostOf "hosts" "misnamed" (p: [ p.hello ]) { name = "other"; };
      nixosConfigurations.elsewhere = mkHostOf "hosts" "elsewhere" (p: [ p.hello ]) { pinsFile = ./sub/pins.json; };
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
nixheal_ switch --update || fail "nixheal update gave up on r1"
expect_log "created $sys/pins.json"
git -C "$sys" ls-files --error-unmatch pins.json >/dev/null || fail "pins.json is not tracked"
git -C "$sys" diff --quiet HEAD -- pins.json || fail "pins.json is not committed"
expect_switched_to_built a
expect_pins a ""
expect_pins b ""

step "r2: libfoo breaks (only b uses app) and gone is removed (only a uses it)"
revision broken true 2 ok
before_calls="$(nh_calls)"
nixheal_ switch --update || fail "nixheal update gave up on r2"
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
nixheal_ why app --host b || fail "nixheal why failed"
expect_log "-libfoo-1.0.drv"
expect_log "trail:    libfoo-1.0 -> app-2.0"
expect_log "libfoo does not compile"

step "r2 switch: no update, so nothing is retried and nothing is committed"
before_commits="$(commits)"
nixheal_ switch || fail "nixheal switch gave up on r2"
reject_log "retrying"
[ "$(commits)" = "$before_commits" ] || fail "switch committed without changes"
expect_switched_to_built a

step "r3: libfoo is fixed, gone is still removed"
revision fixed true 3 ok
nixheal_ switch --update || fail "nixheal update gave up on r3"
expect_log "unpinned host b: toypkgs.app: builds again"
expect_log "still failing host a: toypkgs.gone"
expect_pins a "toypkgs.gone"
expect_pins b ""
[ "$(field a toypkgs gone since)" = "$since_gone" ] || fail "gone lost its original date"
r3_lock="$work/r3.lock"
cp "$sys/flake.lock" "$r3_lock"

step "r4: linux breaks on b (denylisted): b is held on the r3 lock, a moves on"
revision fixed true 4 broken
nixheal_ switch --update || fail "nixheal update gave up on r4"
expect_log "held host b on its last good lock"
case "$(held_reason b)" in *"linux fails but is never pinned"*) ;; *) fail "b should be held for linux: $(held_reason b)" ;; esac
held_rev="$(jq_pins 'f.hosts.b.held.lock.nodes.toypkgs.locked.rev')"
r3_rev="$(nix eval --raw --impure --expr "(builtins.fromJSON (builtins.readFile $r3_lock)).nodes.toypkgs.locked.rev")"
[ "$held_rev" = "$r3_rev" ] || fail "b is held on $held_rev, expected the r3 lock $r3_rev"
expect_built a hello-4
nixheal_ status || fail "nixheal status failed"
expect_log "host b	held	since"

step "r5: linux is fixed and gone is back: b is released, a unpins"
revision fixed false 5 ok
nixheal_ switch --update || fail "nixheal update gave up on r5"
expect_log "released host b"
expect_log "unpinned host a: toypkgs.gone: builds again"
expect_pins a ""
expect_pins b ""
[ -z "$(held_reason b)" ] || fail "b is still held"

step "r5 quiet switch: nothing changes, so nothing is printed"
nixheal_ switch --quiet || fail "the quiet switch gave up"
reject_log "round 1:"
reject_log "builds"
[ -z "$(grep 'nixheal:' "$log" || true)" ] || fail "the quiet switch printed: $(cat "$log")"

step "r6: hello breaks on a, the local host, and is denylisted: nothing changes"
write_config '"hello"'
revision fixed false broken ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
before_commits="$(commits)"
before_calls="$(nh_calls)"
if nixheal_ switch --update; then fail "nixheal update should have given up on r6"; fi
expect_log "giving up on host a"
expect_log "restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored"
[ "$(commits)" = "$before_commits" ] || fail "a failed update committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "a failed update switched"
write_config ""

step "r6 dry run, quiet: shows the hello pins and nothing else, writes nothing"
nixheal_ -q switch --update --dry-run || fail "the dry run gave up"
reject_log "round 1:"
expect_log "dry run: would have pinned host a: toypkgs.hello"
expect_log "dry run: would have pinned host b: toypkgs.hello"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "the dry run changed flake.lock"
cmp -s "$sys/pins.json" "$work/before.json" || fail "the dry run changed pins.json"
[ "$(commits)" = "$before_commits" ] || fail "the dry run committed"
[ "$(nh_calls)" = "$before_calls" ] || fail "the dry run switched"

step "manual pin: kept by switch, removed by clear"
nixheal_ pin hello --input toypkgs || fail "nixheal pin failed"
expect_pins a "toypkgs.hello"
[ "$(field a toypkgs hello reason)" = manual ] || fail "hello should be a manual pin"
nixheal_ switch || fail "nixheal switch gave up with a manual pin"
reject_log "retrying"
expect_pins a "toypkgs.hello"
git -C "$sys" log -1 --format=%s | grep -qF "nixheal: update pins" || fail "switch did not commit the manual pin"
nixheal_ clear hello || fail "nixheal clear failed"
expect_log "cleared host a: toypkgs.hello"
expect_pins a ""

step "fleet: gone removed and denylisted; with --hold-local a, the local host, is held and b moves on"
write_config '"gone"'
revision fixed true 7 ok
before_calls="$(nh_calls)"
before_commits="$(commits)"
nixheal_ build --update --hold-local --no-commit --gc-roots "$work/roots" || fail "the fleet run gave up"
expect_log "held host a on its last good lock"
reject_log "giving up on host a"
case "$(held_reason a)" in *"gone fails but is never pinned"*) ;; *) fail "a should be held for gone: $(held_reason a)" ;; esac
[ -z "$(held_reason b)" ] || fail "b should not be held"
[ "$(nh_calls)" = "$before_calls" ] || fail "a fleet run switched"
[ "$(commits)" = "$before_commits" ] || fail "a --no-commit run committed"
git -C "$sys" diff --quiet -- flake.lock && fail "the fleet run restored flake.lock"
[ -L "$work/roots/b" ] || fail "no GC root for b"
[ -e "$work/roots/a" ] && fail "a GC root for the held host a"
expect_log "rooted 1 system(s)"
write_config ""

step "merge: an entry from a newer lock replaces b, one from an older lock does not"
mkpins() {
  nix eval --impure --json --expr "
    let f = builtins.fromJSON (builtins.readFile $sys/pins.json); b = f.hosts.b; in
    f // { hosts = f.hosts // { b = b // { lockLastModified = b.lockLastModified + ($1); pins.toypkgs.$2 = {
      locked = { type = \"path\"; path = \"/x\"; narHash = \"sha256-x\"; }; since = \"2026-01-01\"; reason = \"manual\"; checkedNarHash = \"\"; }; }; }; }" >"$3"
}
mkpins -100 older "$work/older.json"
mkpins 100 newer "$work/newer.json"
nixheal_ merge "$work/older.json" || fail "merge failed"
expect_log "kept host b: it comes from a newer lock"
expect_pins b ""
nixheal_ merge "$work/newer.json" || fail "merge failed"
expect_log "took host b from"
expect_pins b "toypkgs.newer"
case "$(held_reason a)" in *"gone"*) ;; *) fail "merge lost a's held entry" ;; esac

step "build: a builds, --built-link points at it, nothing is activated"
printf '{"version":2}\n' >"$sys/pins.json"
write_config ""
revision fixed false 8 ok
before_calls="$(nh_calls)"
nixheal_ build --update --built-link "$work/built" || fail "nixheal build gave up"
want_a="$(nix eval --raw "$sys#nixosConfigurations.a.config.system.build.toplevel.outPath")"
[ "$(readlink "$work/built")" = "$want_a" ] || fail "--built-link does not point at a's system: $(readlink "$work/built")"
[ "$(nh_calls)" = "$before_calls" ] || fail "nixheal build activated something"
expect_built a hello-8

step "build: without --hold-local a failing local host gives up and removes --built-link"
write_config '"hello"'
revision fixed false broken ok
cp "$sys/flake.lock" "$work/before.lock"
if nixheal_ build --update --built-link "$work/built"; then fail "nixheal build should have given up"; fi
expect_log "giving up on host a"
[ -e "$work/built" ] && fail "--built-link survived a failed build"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored"
write_config ""
revision fixed false 9 ok
nixheal_ build --update || fail "nixheal build gave up on hello-9"

step "boot: nh os boot gets the built system"
nixheal_ boot || fail "nixheal boot failed"
want_a="$(nix eval --raw "$sys#nixosConfigurations.a.config.system.build.toplevel.outPath")"
[ "$(tail -n1 "$calls")" = "nh os boot $want_a" ] || fail "boot did not call nh os boot: $(tail -n1 "$calls")"

step "native switcher: nixos-rebuild switches the flake"
write_config "" '"switcher": "native"'
nixheal_ switch || fail "nixheal switch with the native switcher failed"
[ "$(tail -n1 "$calls")" = "nixos-rebuild switch --flake $sys#a --sudo" ] || fail "nixos-rebuild was not called: $(tail -n1 "$calls")"

step "home: a standalone Home Manager configuration switches through nh home"
write_home_config
nixheal_ switch || fail "nixheal switch for the home failed"
want_home="$(nix eval --raw "$sys#homeConfigurations.alice.activationPackage.outPath")"
[ "$(tail -n1 "$calls")" = "nh home switch $want_home" ] || fail "nh home switch was not given the home: $(tail -n1 "$calls")"
expect_log "home alice builds"
if nixheal_ boot; then fail "boot should refuse a home"; fi
expect_log "Home Manager has no boot activation"
reject_log "round 1"
write_config ""

step "probe: hosts whose pins could never apply are refused before anything builds"
write_config "" "" '"misnamed"'
if nixheal_ build; then fail "a misnamed host should be refused"; fi
expect_log 'has programs.nixheal.name = "other"; it must be "misnamed"'
reject_log "round 1"
write_config "" "" '"elsewhere"'
if nixheal_ build; then fail "a pinsFile outside the flake root should be refused"; fi
expect_log "pinsFile = sub/pins.json; it must be pins.json at the flake root"
reject_log "round 1"
write_config ""

step "--only: an unknown name is an error, not a silent no-op"
if nixheal_ build --only nope; then fail "--only nope should fail"; fi
expect_log "--only nope is not among the configured hosts and homes (a, b)"

step "dry run on a fresh checkout: pins.json is created for the build and removed again, --built-link is left alone"
cp "$sys/pins.json" "$work/pins.keep"
git -C "$sys" rm -q pins.json
git -C "$sys" commit -qm "no pins.json"
ln -sfn /nonexistent "$work/keep"
nixheal_ build --dry-run --built-link "$work/keep" || fail "the dry run on a fresh checkout failed"
[ -e "$sys/pins.json" ] && fail "the dry run left pins.json behind"
git -C "$sys" ls-files --error-unmatch pins.json >/dev/null 2>&1 && fail "the dry run left pins.json in the index"
[ "$(readlink "$work/keep")" = /nonexistent ] || fail "the dry run touched --built-link"
cp "$work/pins.keep" "$sys/pins.json"
git -C "$sys" add pins.json
git -C "$sys" commit -qm "pins.json again"

step "switch --hold-local: a held local host is not activated"
write_config '"hello"'
revision fixed false broken ok
before_calls="$(nh_calls)"
nixheal_ switch --update --hold-local --no-commit || fail "switch --hold-local gave up"
expect_log "held host a on its last good lock"
expect_log "not activating: host a is held"
[ "$(nh_calls)" = "$before_calls" ] || fail "a held local host was activated"
git -C "$sys" checkout -q -- flake.lock pins.json
write_config ""

step "interrupted: SIGTERM during the build restores flake.lock and pins.json"
revision fixed false slow ok
cp "$sys/flake.lock" "$work/before.lock"
cp "$sys/pins.json" "$work/before.json"
"$nixheal" switch --update --config "$config" >"$work/stdout" 2>"$log" &
pid=$!
for _ in $(seq 60); do
  grep -q "round 1" "$log" && break
  sleep 0.5
done
grep -q "round 1" "$log" || fail "the slow build never started"
sleep 2
kill -TERM "$pid"
set +e
wait "$pid"
rc=$?
set -e
[ "$rc" = 130 ] || fail "an interrupted run should exit 130, not $rc"
expect_log "interrupted; restored flake.lock and pins.json"
cmp -s "$sys/flake.lock" "$work/before.lock" || fail "flake.lock was not restored after SIGTERM"
cmp -s "$sys/pins.json" "$work/before.json" || fail "pins.json was not restored after SIGTERM"
revision fixed false 10 ok
nixheal_ build --update || fail "nixheal build gave up after the interrupted run"

printf '\nall e2e steps passed\n'
