self:
{
  config,
  lib,
  pkgs,
  osConfig ? null,
  ...
}:
let
  cfg = config.programs.autopin;
  inNixos = osConfig != null;
  osCfg = if inNixos then osConfig.programs.autopin or { } else { };
  globalPkgs = inNixos && (osConfig.home-manager.useGlobalPkgs or false);
  nhFlake = config.programs.nh.flake or null;

  overlay =
    input: nixpkgsConfig:
    self.lib.mkOverlay {
      pins = cfg.pinsFile;
      kind = if inNixos then "hosts" else "homes";
      inherit (cfg) name;
      inherit input nixpkgsConfig;
    };

  extraSets = lib.mapAttrs (
    input: set:
    import set.source {
      inherit (pkgs.stdenv.hostPlatform) system;
      inherit (set) config;
      overlays = lib.optional cfg.enable (overlay input set.config);
    }
  ) cfg.nixpkgs;

  runsAutopin = !inNixos && (cfg.hosts != [ ] || cfg.homes != [ ]);
in
{
  options.programs.autopin = import ./options.nix {
    inherit lib self pkgs;
    defaultName = if inNixos then osConfig.networking.hostName else config.home.username;
    defaultEnable = osCfg.enable or false;
    defaultPinsFile = osCfg.pinsFile or null;
    defaultFlake = if nhFlake == null then null else toString nhFlake;
    defaultSetConfig = config.nixpkgs.config or { };
  };

  config = lib.mkMerge [
    {
      programs.autopin.packageSets = {
        nixpkgs = pkgs;
      }
      // extraSets;
      _module.args = lib.mapAttrs' (input: set: lib.nameValuePair set.argName extraSets.${input}) (
        lib.filterAttrs (_: set: set.argName != null) cfg.nixpkgs
      );
    }
    (lib.mkIf (cfg.enable && !globalPkgs) {
      assertions = [
        {
          assertion = cfg.pinsFile != null;
          message = "programs.autopin.pinsFile must point at the flake's pins.json";
        }
      ];
      nixpkgs.overlays = lib.mkAfter [ (overlay "nixpkgs" (config.nixpkgs.config or { })) ];
    })
    (lib.mkIf (cfg.enable && runsAutopin) {
      home.packages = [ cfg.package ];
      xdg.configFile."autopin.json".text = builtins.toJSON {
        kind = "home";
        self = cfg.name;
        inherit (cfg)
          flake
          hosts
          homes
          warnAfterDays
          deny
          quiet
          ;
        inputs = builtins.attrNames cfg.packageSets;
      };
    })
  ];
}
