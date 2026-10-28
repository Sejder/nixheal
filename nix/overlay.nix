{
  pins,
  kind ? "hosts",
  name,
  input,
  nixpkgsConfig ? null,
  overlays ? [ ],
}:
final: prev:
let
  data = if pins != null && builtins.pathExists pins then builtins.fromJSON (builtins.readFile pins) else { };
  entry = (data.${kind} or { }).${name} or { };
  set = (entry.pins or { }).${input} or { };

  importPinned = import ./import-pinned.nix {
    config = if nixpkgsConfig != null then nixpkgsConfig else prev.config;
    lib = prev.lib or null;
    inherit (prev.stdenv) buildPlatform hostPlatform;
  };
  carried = pin: pin.overlays or false;
  key = pin: "${pin.locked.narHash}${if carried pin then "+overlays" else ""}";

  sources = builtins.listToAttrs (
    map (pin: {
      name = key pin;
      value = importPinned {
        inherit (pin) locked;
        overlays = if carried pin then overlays else [ ];
      };
    }) (builtins.attrValues set)
  );
in
builtins.mapAttrs (attr: pin: sources.${key pin}.${attr}) set
