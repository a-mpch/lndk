{ self, inputs, ... }:
{
  perSystem =
    { system, pkgs, ... }:
    let
      rustPackages = import ./rust.nix { inherit pkgs inputs; };
    in
    {
      packages = {
        inherit (rustPackages) rust lndk-itest-env;
        lndk = rustPackages.rust; # Alias for consistency
        default = rustPackages.rust;
      };
    };
}
