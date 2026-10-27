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
          failures = builtins.toFile "failures.json" (
            builtins.toJSON {
              pins.nixpkgs.hello = {
                inherit locked;
                since = "2026-01-01";
                reason = "build";
                checkedNarHash = "";
              };
            }
          );
          prev = {
            stdenv.hostPlatform.system = system;
            config = { };
            hello = "new";
            cowsay = "new";
          };
          pinned = self.lib.mkOverlay {
            inherit failures;
            input = "nixpkgs";
          } prev prev;
          other = self.lib.mkOverlay {
            inherit failures;
            input = "nixpkgs-unstable";
          } prev prev;
          missing = self.lib.mkOverlay {
            failures = ./does-not-exist.json;
            input = "nixpkgs";
          } prev prev;
        in
        {
          package = self.packages.${system}.default;
          overlay =
            assert pinned.hello.drvPath == pkgs.hello.drvPath;
            assert !(pinned ? cowsay);
            assert other == { };
            assert missing == { };
            pkgs.runCommand "autopin-overlay-check" { } "touch $out";
        }
      );
    };
}
