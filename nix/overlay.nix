{
  pins,
  kind ? "hosts",
  name,
  input,
  nixpkgsConfig ? null,
}:
final: prev:
let
  data = if pins != null && builtins.pathExists pins then builtins.fromJSON (builtins.readFile pins) else { };
  entry = (data.${kind} or { }).${name} or { };
  set = (entry.pins or { }).${input} or { };

  given = if nixpkgsConfig != null then nixpkgsConfig else prev.config;
  importConfig = builtins.removeAttrs given (
    builtins.filter (n: given.${n} == null) (builtins.attrNames given)
  );

  sources = builtins.listToAttrs (
    map (pin: {
      name = pin.locked.narHash;
      value = import (builtins.fetchTree pin.locked) (
        {
          config = importConfig;
          overlays = [ ];
        }
        // import ./platform.nix (prev.lib or null) prev.stdenv.buildPlatform prev.stdenv.hostPlatform
      );
    }) (builtins.attrValues set)
  );
in
builtins.mapAttrs (attr: pin: sources.${pin.locked.narHash}.${attr}) set
