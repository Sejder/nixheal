{
  config,
  lib ? null,
  buildPlatform,
  hostPlatform,
}:
{
  locked,
  overlays ? [ ],
}:
import (builtins.fetchTree locked) (
  {
    config = builtins.removeAttrs config (
      builtins.filter (n: config.${n} == null) (builtins.attrNames config)
    );
    inherit overlays;
  }
  // import ./platform.nix lib buildPlatform hostPlatform
)
