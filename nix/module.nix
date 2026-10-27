self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.nixheal;
  nhFlake = config.programs.nh.flake or null;

  overlay =
    input: nixpkgsConfig:
    self.lib.mkOverlay {
      pins = cfg.pinsFile;
      kind = "hosts";
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

  runsNixheal = cfg.hosts != [ ] || cfg.homes != [ ];
in
{
  options.programs.nixheal = import ./options.nix {
    inherit lib self pkgs;
    defaultName = config.networking.hostName;
    defaultEnable = false;
    defaultPinsFile = null;
    defaultFlake = if nhFlake == null then null else toString nhFlake;
    defaultSetConfig = config.nixpkgs.config;
  };

  config = lib.mkMerge [
    {
      programs.nixheal.packageSets = {
        nixpkgs = pkgs;
      }
      // extraSets;
      _module.args = lib.mapAttrs' (input: set: lib.nameValuePair set.argName extraSets.${input}) (
        lib.filterAttrs (_: set: set.argName != null) cfg.nixpkgs
      );
    }
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.pinsFile != null;
          message = "programs.nixheal.pinsFile must point at the flake's pins.json";
        }
      ];
      nixpkgs.overlays = lib.mkAfter [ (overlay "nixpkgs" config.nixpkgs.config) ];
    })
    (lib.mkIf (cfg.enable && runsNixheal) {
      environment.systemPackages = [ cfg.package ];
      environment.etc."nixheal.json".text = builtins.toJSON {
        kind = "nixos";
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
