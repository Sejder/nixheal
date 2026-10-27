self:
{
  config,
  lib,
  pkgs,
  osConfig ? null,
  ...
}:
let
  cfg = config.programs.nixheal;
  inNixos = osConfig != null;
  osCfg = if inNixos then osConfig.programs.nixheal or { } else { };
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

  runsNixheal = !inNixos && (cfg.hosts != [ ] || cfg.homes != [ ]);
  reservedArgs = [
    "pkgs"
    "lib"
    "config"
    "options"
    "osConfig"
    "specialArgs"
  ];
in
{
  options.programs.nixheal = import ./options.nix {
    inherit lib self pkgs;
    defaultName = if inNixos then osConfig.networking.hostName else config.home.username;
    defaultEnable = osCfg.enable or false;
    defaultInput = osCfg.input or "nixpkgs";
    defaultPinsFile = osCfg.pinsFile or null;
    defaultFlake = if nhFlake == null then null else toString nhFlake;
    defaultSetConfig = config.nixpkgs.config or { };
  };

  config = lib.mkMerge [
    {
      programs.nixheal.packageSets = {
        ${cfg.input} = pkgs;
      }
      // extraSets;
      warnings = lib.optional (
        inNixos && (cfg.hosts != [ ] || cfg.homes != [ ] || cfg.autoUpgrade.enable)
      ) "programs.nixheal.hosts, homes and autoUpgrade in Home Manager only apply to a standalone Home Manager; inside NixOS, set them in the NixOS configuration";
      _module.args = lib.mapAttrs' (input: set: lib.nameValuePair set.argName extraSets.${input}) (
        lib.filterAttrs (_: set: set.argName != null) cfg.nixpkgs
      );
    }
    (lib.mkIf (cfg.enable && !globalPkgs) {
      assertions = [
        {
          assertion = cfg.pinsFile != null;
          message = "programs.nixheal.pinsFile must point at the flake's pins.json";
        }
        {
          assertion =
            cfg.pinsFile == null
            || builtins.isPath cfg.pinsFile
            || lib.hasPrefix "${builtins.storeDir}/" (toString cfg.pinsFile);
          message = "programs.nixheal.pinsFile must be a path such as ./pins.json, not a string";
        }
      ]
      ++ lib.mapAttrsToList (input: set: {
        assertion = set.argName == null || !(builtins.elem set.argName reservedArgs);
        message = "programs.nixheal.nixpkgs.${input}.argName = \"${toString set.argName}\" would shadow a module argument";
      }) cfg.nixpkgs;
      nixpkgs.overlays = lib.mkAfter [ (overlay cfg.input (config.nixpkgs.config or { })) ];
    })
    (lib.mkIf (cfg.enable && runsNixheal) {
      home.packages = [ cfg.package ];
      xdg.configFile."nixheal.json".text = builtins.toJSON {
        kind = "home";
        self = cfg.name;
        inherit (cfg)
          flake
          hosts
          homes
          warnAfterDays
          deny
          quiet
          switcher
          ;
      };
    })
    (lib.mkIf (cfg.enable && cfg.autoUpgrade.enable && runsNixheal) {
      assertions = [
        {
          assertion = builtins.elem cfg.name cfg.homes;
          message = "programs.nixheal.autoUpgrade needs this home (${cfg.name}) in programs.nixheal.homes";
        }
      ];

      systemd.user.services.nixheal-upgrade = {
        Unit = {
          Description = "Update the flake with nixheal and switch Home Manager";
        };
        Service = {
          Type = "oneshot";
          Environment = [ "PATH=${config.home.profileDirectory}/bin:/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin" ];
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe cfg.package)
              "switch"
            ]
            ++ lib.optional (!cfg.autoUpgrade.commit) "--no-commit"
            ++ [ "--update" ]
            ++ cfg.autoUpgrade.inputs
          );
        };
      };

      systemd.user.timers.nixheal-upgrade = {
        Unit.Description = "Scheduled nixheal upgrade";
        Install.WantedBy = [ "timers.target" ];
        Timer = {
          OnCalendar = cfg.autoUpgrade.dates;
          RandomizedDelaySec = cfg.autoUpgrade.randomizedDelaySec;
          Persistent = true;
        };
      };
    })
  ];
}
