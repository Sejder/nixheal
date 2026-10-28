{
  lib,
  self,
  pkgs,
  defaultName,
  defaultEnable,
  defaultPinsFile,
  defaultFlake,
  defaultSetConfig,
  defaultInput ? "nixpkgs",
}:
let
  inherit (lib) mkOption types;
  nixpkgsSet = types.submodule {
    options = {
      source = mkOption {
        type = types.raw;
        description = "A nixpkgs source, usually a flake input.";
      };
      config = mkOption {
        type = types.attrs;
        default = defaultSetConfig;
        description = "The nixpkgs config to import it with.";
      };
      argName = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Also pass the imported set to modules under this argument name.";
      };
    };
  };
in
{
  enable = mkOption {
    type = types.bool;
    default = defaultEnable;
    description = "Apply this machine's nixheal pins, and install nixheal when it builds hosts or homes.";
  };
  package = mkOption {
    type = types.package;
    default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    defaultText = lib.literalExpression "nixheal.packages.\${system}.default";
    description = "The nixheal package.";
  };
  input = mkOption {
    type = types.str;
    default = defaultInput;
    example = "nixos";
    description = "The flake input this machine's pkgs come from, as named in flake.lock; pins for pkgs are kept under it.";
  };
  pinsFile = mkOption {
    type = types.nullOr types.path;
    default = defaultPinsFile;
    example = lib.literalExpression "./pins.json";
    description = "The flake's pins.json. It may not exist yet; nixheal creates it.";
  };
  name = mkOption {
    type = types.str;
    default = defaultName;
    description = "The entry in pins.json that holds this machine's pins.";
  };
  flake = mkOption {
    type = types.nullOr types.str;
    default = defaultFlake;
    description = "Path of the flake checkout that nixheal updates.";
  };
  hosts = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "nixosConfigurations that nixheal builds and pins when it runs here.";
  };
  homes = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "homeConfigurations that nixheal builds and pins when it runs here.";
  };
  warnAfterDays = mkOption {
    type = types.ints.unsigned;
    default = 14;
    description = "Log a pin at warning level once it is older than this.";
  };
  deny = mkOption {
    type = types.listOf types.str;
    default = [ ];
    example = [ "cuda*" ];
    description = "Attributes never to pin, on top of the built-in list. A trailing * matches a prefix.";
  };
  switcher = mkOption {
    type = types.enum [
      "nh"
      "native"
    ];
    default = "nh";
    description = "How nixheal switch and boot activate: nh, or native (nixos-rebuild for hosts, home-manager for homes).";
  };
  autoUpgrade = {
    enable = lib.mkEnableOption "a timer that updates the flake with nixheal and activates the result";
    dates = mkOption {
      type = types.str;
      default = "04:40";
      example = "daily";
      description = "When to run, as a systemd calendar expression.";
    };
    randomizedDelaySec = mkOption {
      type = types.str;
      default = "45min";
      description = "Random delay added to each run.";
    };
    inputs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "nixpkgs" ];
      description = "Flake inputs to update; all of them when empty.";
    };
    commit = mkOption {
      type = types.bool;
      default = true;
      description = "Commit flake.lock and pins.json after each run.";
    };
    onFailure = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "notify-admin@%n.service" ];
      description = "Units to start when a run fails, e.g. to send a notification.";
    };
  };
  quiet = mkOption {
    type = types.bool;
    default = false;
    description = "Only report pins, unpins, holds, warnings and errors, as with --quiet.";
  };
  nixpkgs = mkOption {
    type = types.attrsOf nixpkgsSet;
    default = { };
    example = lib.literalExpression ''
      {
        nixpkgs-unstable = {
          source = inputs.nixpkgs-unstable;
          argName = "unstable-pkgs";
        };
      }
    '';
    description = "Further nixpkgs to import with pins applied, keyed by their flake input name.";
  };
  packageSets = mkOption {
    type = types.attrsOf types.raw;
    readOnly = true;
    description = "Every package set nixheal pins on this machine, keyed by flake input.";
  };
}
