{
  lib,
  self,
  pkgs,
  defaultName,
  defaultEnable,
  defaultPinsFile,
  defaultFlake,
  defaultSetConfig,
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
    description = "Apply this machine's autopin pins, and install autopin when it builds hosts or homes.";
  };
  package = mkOption {
    type = types.package;
    default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
    defaultText = lib.literalExpression "autopin.packages.\${system}.default";
    description = "The autopin package.";
  };
  pinsFile = mkOption {
    type = types.nullOr types.path;
    default = defaultPinsFile;
    example = lib.literalExpression "./pins.json";
    description = "The flake's pins.json. It may not exist yet; autopin creates it.";
  };
  name = mkOption {
    type = types.str;
    default = defaultName;
    description = "The entry in pins.json that holds this machine's pins.";
  };
  flake = mkOption {
    type = types.nullOr types.str;
    default = defaultFlake;
    description = "Path of the flake checkout that autopin updates.";
  };
  hosts = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "nixosConfigurations that autopin builds and pins when it runs here.";
  };
  homes = mkOption {
    type = types.listOf types.str;
    default = [ ];
    description = "homeConfigurations that autopin builds and pins when it runs here.";
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
    description = "Every package set autopin pins on this machine, keyed by flake input.";
  };
}
