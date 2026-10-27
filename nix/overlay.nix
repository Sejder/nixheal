{
  pins,
  kind ? "hosts",
  name,
  input,
}:
final: prev:
let
  data = if pins != null && builtins.pathExists pins then builtins.fromJSON (builtins.readFile pins) else { };
  entry = (data.${kind} or { }).${name} or { };
  set = (entry.pins or { }).${input} or { };
  sources = builtins.listToAttrs (
    map (pin: {
      name = pin.locked.narHash;
      value = import (builtins.fetchTree pin.locked) {
        inherit (prev.stdenv.hostPlatform) system;
        inherit (prev) config;
      };
    }) (builtins.attrValues set)
  );
in
builtins.mapAttrs (attr: pin: sources.${pin.locked.narHash}.${attr}) set
