{
  description = "Pin NixOS packages that fail to build to their last good nixpkgs";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      lib.mkOverlay = import ./nix/overlay.nix;
      nixosModules.default = import ./nix/module.nix self;
      homeManagerModules.default = import ./nix/hm-module.nix self;

      packages = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "autopin";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = pkgs.lib.fileset.toSource {
              root = ./.;
              fileset = pkgs.lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./src
                ./tests
              ];
            };
            cargoLock.lockFile = ./Cargo.lock;
            meta.mainProgram = "autopin";
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          locked = (builtins.fromJSON (builtins.readFile ./flake.lock)).nodes.nixpkgs.locked;
          pin = {
            inherit locked;
            since = "2026-01-01";
            reason = "build";
            checkedNarHash = "";
          };
          pins = builtins.toFile "pins.json" (
            builtins.toJSON {
              version = 2;
              hosts.test.pins.nixpkgs.hello = pin;
              homes.mikke.pins.nixpkgs.cowsay = pin;
            }
          );
          prev = {
            stdenv.hostPlatform.system = system;
            config = { };
            hello = "new";
            cowsay = "new";
          };
          overlay =
            args:
            self.lib.mkOverlay (
              {
                inherit pins;
                name = "test";
                input = "nixpkgs";
              }
              // args
            ) prev prev;

          machine = nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.default
              {
                networking.hostName = "test";
                programs.autopin = {
                  enable = true;
                  pinsFile = pins;
                  hosts = [ "test" ];
                  nixpkgs.extra = {
                    source = nixpkgs;
                    argName = "extra-pkgs";
                  };
                };
                nixpkgs.overlays = [
                  (final: prev: {
                    hello = prev.hello.overrideAttrs { pname = "hello-local"; };
                    cowsay = prev.cowsay.overrideAttrs { pname = "cowsay-local"; };
                  })
                ];
                boot.loader.grub.enable = false;
                fileSystems."/" = {
                  device = "none";
                  fsType = "tmpfs";
                };
                system.stateVersion = nixpkgs.lib.trivial.release;
              }
              (
                { extra-pkgs, ... }:
                {
                  environment.etc."extra-hello".text = extra-pkgs.hello.pname;
                }
              )
            ];
          };
          settings = builtins.fromJSON machine.config.environment.etc."autopin.json".text;
        in
        {
          package = self.packages.${system}.default;
          overlay =
            assert (overlay { }).hello.drvPath == pkgs.hello.drvPath;
            assert !((overlay { }) ? cowsay);
            assert overlay { input = "nixpkgs-unstable"; } == { };
            assert overlay { name = "other"; } == { };
            assert (overlay { kind = "homes"; name = "mikke"; }) ? cowsay;
            assert overlay { pins = ./does-not-exist.json; } == { };
            assert overlay { pins = null; } == { };
            pkgs.runCommand "autopin-overlay-check" { } "touch $out";
          module =
            assert machine.pkgs.hello.pname == "hello";
            assert machine.pkgs.cowsay.pname == "cowsay-local";
            assert machine.config.programs.autopin.packageSets ? extra;
            assert machine.config.environment.etc."extra-hello".text == "hello";
            assert settings.self == "test";
            assert settings.kind == "nixos";
            assert settings.inputs == [ "extra" "nixpkgs" ];
            assert builtins.elem machine.config.programs.autopin.package machine.config.environment.systemPackages;
            pkgs.runCommand "autopin-module-check" { } "touch $out";
        }
      );
    };
}
