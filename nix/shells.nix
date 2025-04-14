{ self, ... }:
{
  perSystem =
    {
      config,
      pkgs,
      system,
      ...
    }:
    {
      devShells = {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            rust-analyzer
            pkg-config
            protobuf
            openssl
            openssl.dev
          ];

          inputsFrom = [
            config.packages.lndk
          ];

          shellHook = ''
            echo "LNDK development environment loaded"
          '';
        };
      };
    };
}
