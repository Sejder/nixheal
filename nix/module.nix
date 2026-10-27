self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.autopin;
  nhFlake = config.programs.nh.flake or null;

  overlay =
    input:
    self.lib.mkOverlay {
      pins = cfg.pinsFile;
      kind = "hosts";
      inherit (cfg) name;
      inherit input;
    };

  extraSets = lib.mapAttrs (
    input: set:
    import set.source {
      inherit (pkgs.stdenv.hostPlatform) system;
      inherit (set) config;
      overlays = lib.optional cfg.enable (overlay input);
    }
  ) cfg.nixpkgs;

  runsAutopin = cfg.hosts != [ ] || cfg.homes != [ ];
in
{
  options.programs.autopin = import ./options.nix {
    inherit lib self pkgs;
    defaultName = config.networking.hostName;
    defaultEnable = false;
    defaultPinsFile = null;
    defaultFlake = if nhFlake == null then null else toString nhFlake;
    defaultSetConfig = config.nixpkgs.config;
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
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.pinsFile != null;
          message = "programs.autopin.pinsFile must point at the flake's pins.json";
        }
      ];
      nixpkgs.overlays = lib.mkAfter [ (overlay "nixpkgs") ];
    })
    (lib.mkIf (cfg.enable && runsAutopin) {
      environment.systemPackages = [ cfg.package ];
      environment.etc."autopin.json".text = builtins.toJSON {
        kind = "nixos";
        self = cfg.name;
        inherit (cfg)
          flake
          hosts
          homes
          warnAfterDays
          deny
          ;
        inputs = builtins.attrNames cfg.packageSets;
      };
    })
  ];
}
