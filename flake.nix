{
  description = "LNDK: Standalone deamon that connects to LND that implements bolt12 funtionallities";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";

    flake-parts.url = "github:hercules-ci/flake-parts";

    crane.url = "github:ipetkov/crane";

    treefmt-nix.url = "github:numtide/treefmt-nix";

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = nixpkgs.lib.systems.flakeExposed;
      imports = [
        inputs.treefmt-nix.flakeModule
        ./nix/pkgs/flake-module.nix
        ./nix/checks/flake-module.nix
        ./nix/shells.nix
        ./nix/treefmt.nix
      ];
      perSystem =
        {
          config,
          pkgs,
          self',
          system,
          ...
        }:
        {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (final: prev: { craneLib = (inputs.crane.mkLib pkgs); }) ];
          };
          apps = {
            lndk = {
              program = "${self'.packages.lndk}/bin/lndk";
            };
            lndk-cli = {
              program = "${self'.packages.lndk}/bin/lndk-cli";
            };
          };
        };
    };
}
