{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "koerier";
  version = (lib.importTOML ../Cargo.toml).package.version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions (
      [
        ../Cargo.toml
        ../Cargo.lock
        ../src
        ../assets
        ../LICENSE-MIT
        ../LICENSE-APACHE
        ../README.md
      ]
      ++ lib.optional (builtins.pathExists ../tests) ../tests
    );
  };

  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    description = "Lightning Address and LNURL-pay server backed by LND";
    homepage = "https://github.com/tee8z/koerier";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "koerier";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
