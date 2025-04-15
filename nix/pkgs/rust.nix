{ pkgs, inputs, ... }:

let
  inherit (pkgs) lib stdenv;
  src = ../../.;

  commonDeps = {
    nativeBuildInputs = with pkgs; [
      pkg-config
      protobuf
    ];

    buildInputs = with pkgs; [
      openssl
    ];
  };

  cargoArtifacts = pkgs.craneLib.buildDepsOnly {
    inherit src;
    pname = "lndk-deps";
    inherit (commonDeps) nativeBuildInputs buildInputs;
  };

  basePkg = {
    inherit src cargoArtifacts;
    inherit (commonDeps) nativeBuildInputs buildInputs;

    meta = with lib; {
      description = "Standalone daemon that connects to LND to implement bolt12 functionalities";
      homepage = "https://github.com/lndk-org/lndk";
      license = licenses.mit;
      platforms = platforms.linux ++ platforms.darwin;
    };
  };

  lndkPkg = pkgs.craneLib.buildPackage (
    basePkg
    // {
      pname = "lndk";
    }
  );

  lndkITestPkg = pkgs.craneLib.buildPackage (
    basePkg
    // {
      pname = "lndk-itest";
      cargoExtraArgs = "--features itest";
      RUSTFLAGS = "--cfg itest";
    }
  );
in
{
  rust = lndkPkg;

  lndk-itest-env = pkgs.buildEnv {
    name = "lndk-itest-env";

    paths = [
      lndkITestPkg
      pkgs.go
      pkgs.git
    ];

    meta = with lib; {
      description = "Complete environment for running LNDK integration tests";
      homepage = "https://github.com/lndk-org/lndk";
      license = licenses.mit;
      platforms = platforms.linux ++ platforms.darwin;
    };
  };
}
