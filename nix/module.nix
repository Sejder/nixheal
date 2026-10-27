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

  up = cfg.autoUpgrade;
  builtLink = "/run/nixheal/system";
  activateScript = pkgs.writeShellScript "nixheal-activate" ''
    set -eu
    [ -L ${builtLink} ] || exit 0
    system="$(${pkgs.coreutils}/bin/readlink -f ${builtLink})"
    if [ "${up.operation}" = switch ] && [ "$system" = "$(${pkgs.coreutils}/bin/readlink -f /run/current-system)" ]; then
      echo "already running $system"
      exit 0
    fi
    ${config.nix.package}/bin/nix-env -p /nix/var/nix/profiles/system --set "$system"
    exec "$system/bin/switch-to-configuration" ${up.operation}
  '';
in
{
  options.programs.nixheal =
    lib.recursiveUpdate
      (import ./options.nix {
        inherit lib self pkgs;
        defaultName = config.networking.hostName;
        defaultEnable = false;
        defaultPinsFile = null;
        defaultFlake = if nhFlake == null then null else toString nhFlake;
        defaultSetConfig = config.nixpkgs.config;
      })
      {
        autoUpgrade = {
          operation = lib.mkOption {
            type = lib.types.enum [
              "switch"
              "boot"
            ];
            default = "switch";
            description = "Activate the new system right away, or on the next boot.";
          };
          user = lib.mkOption {
            type = lib.types.str;
            default = "root";
            example = "alice";
            description = "Who updates, builds and commits. Use the owner of the flake checkout, so its files and git objects stay theirs; activation always runs as root.";
          };
        };
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
          switcher
          ;
        inputs = builtins.attrNames cfg.packageSets;
      };
    })
    (lib.mkIf (cfg.enable && up.enable) {
      assertions = [
        {
          assertion = builtins.elem cfg.name cfg.hosts;
          message = "programs.nixheal.autoUpgrade needs this host (${cfg.name}) in programs.nixheal.hosts";
        }
        {
          assertion = cfg.flake != null;
          message = "programs.nixheal.autoUpgrade needs programs.nixheal.flake";
        }
      ];
      warnings = lib.optional config.system.autoUpgrade.enable "programs.nixheal.autoUpgrade and system.autoUpgrade both update this host; disable one";

      systemd.services.nixheal-upgrade = {
        description = "Update the flake with nixheal and activate the result";
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
        restartIfChanged = false;
        stopIfChanged = false;
        path = [
          cfg.package
          config.nix.package
          pkgs.git
          pkgs.coreutils
        ];
        environment = lib.mkIf (up.user == "root") {
          GIT_AUTHOR_NAME = "nixheal";
          GIT_AUTHOR_EMAIL = "nixheal@${config.networking.hostName}";
          GIT_COMMITTER_NAME = "nixheal";
          GIT_COMMITTER_EMAIL = "nixheal@${config.networking.hostName}";
        };
        serviceConfig = {
          Type = "oneshot";
          User = up.user;
          RuntimeDirectory = "nixheal";
          ExecStart = lib.escapeShellArgs (
            [
              (lib.getExe cfg.package)
              "build"
              "--built-link"
              builtLink
            ]
            ++ lib.optional (!up.commit) "--no-commit"
            ++ [ "--update" ]
            ++ up.inputs
          );
          ExecStartPost = "+${activateScript}";
        };
      };

      systemd.timers.nixheal-upgrade = {
        description = "Scheduled nixheal upgrade";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = up.dates;
          RandomizedDelaySec = up.randomizedDelaySec;
          Persistent = true;
        };
      };
    })
  ];
}
