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
            pname = "nixheal";
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
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postInstall = ''
              wrapProgram $out/bin/nixheal --suffix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix-output-monitor ]}
            '';
            meta.mainProgram = "nixheal";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              clippy
              rustc
              rustfmt
              nix-output-monitor
            ];
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
              homes.alice.pins.nixpkgs.cowsay = pin;
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
                programs.nixheal = {
                  enable = true;
                  pinsFile = pins;
                  hosts = [ "test" ];
                  flake = "/home/alice/nixos";
                  autoUpgrade = {
                    enable = true;
                    user = "alice";
                    operation = "boot";
                    inputs = [ "nixpkgs" ];
                  };
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
          settings = builtins.fromJSON machine.config.environment.etc."nixheal.json".text;
          upgrade = machine.config.systemd.services.nixheal-upgrade;
        in
        {
          package = self.packages.${system}.default;
          overlay =
            assert (overlay { }).hello.drvPath == pkgs.hello.drvPath;
            assert !((overlay { }) ? cowsay);
            assert overlay { input = "nixpkgs-unstable"; } == { };
            assert overlay { name = "other"; } == { };
            assert (overlay { kind = "homes"; name = "alice"; }) ? cowsay;
            assert overlay { pins = ./does-not-exist.json; } == { };
            assert overlay { pins = null; } == { };
            assert
              (self.lib.mkOverlay {
                inherit pins;
                name = "test";
                input = "nixpkgs";
              } prev (prev // { config.rewriteURL = null; config.notAnOption = null; })).hello.drvPath
              == pkgs.hello.drvPath;
            assert
              (self.lib.mkOverlay {
                inherit pins;
                name = "test";
                input = "nixpkgs";
                nixpkgsConfig = { };
              } prev prev).hello.drvPath
              == pkgs.hello.drvPath;
            pkgs.runCommand "nixheal-overlay-check" { } "touch $out";
          module =
            assert machine.pkgs.hello.pname == "hello";
            assert machine.pkgs.cowsay.pname == "cowsay-local";
            assert machine.config.programs.nixheal.packageSets ? extra;
            assert machine.config.environment.etc."extra-hello".text == "hello";
            assert settings.self == "test";
            assert settings.kind == "nixos";
            assert settings.quiet == false;
            assert settings.switcher == "nh";
            assert upgrade.serviceConfig.User == "alice";
            assert builtins.match ".*nixheal build --built-link /run/nixheal/system --update nixpkgs" upgrade.serviceConfig.ExecStart != null;
            assert builtins.match "[+]/nix/store/.*-nixheal-activate" upgrade.serviceConfig.ExecStartPost != null;
            assert machine.config.systemd.timers.nixheal-upgrade.timerConfig.OnCalendar == "04:40";
            assert settings.inputs == [ "extra" "nixpkgs" ];
            assert builtins.elem machine.config.programs.nixheal.package machine.config.environment.systemPackages;
            pkgs.runCommand "nixheal-module-check" { } "touch $out";
        }
      );
    };
}
