{ failures, input }:
final: prev:
let
  data = if builtins.pathExists failures then builtins.fromJSON (builtins.readFile failures) else { };
  pins = (data.pins or { }).${input} or { };
  sources = builtins.listToAttrs (
    map (pin: {
      name = pin.locked.narHash;
      value = import (builtins.fetchTree pin.locked) {
        inherit (prev.stdenv.hostPlatform) system;
        inherit (prev) config;
      };
    }) (builtins.attrValues pins)
  );
in
builtins.mapAttrs (attr: pin: sources.${pin.locked.narHash}.${attr}) pins
