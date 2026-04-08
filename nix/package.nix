{ self, ... }:
{
  perSystem =
    {
      lib,
      pkgs,
      self',
      ...
    }:
    let
      mkDate =
        longDate:
        (lib.concatStringsSep "-" [
          (builtins.substring 0 4 longDate)
          (builtins.substring 4 2 longDate)
          (builtins.substring 6 2 longDate)
        ]);
      props = builtins.fromTOML (builtins.readFile ../Cargo.toml);
      pname = "paneru";
      version =
        props.package.version
        + "+date="
        + (mkDate (self.lastModifiedDate or "19700101"))
        + "_"
        + (self.shortRev or "dirty");

    in
    {
      packages.default = self'.packages.paneru;
      packages.paneru = pkgs.rustPlatform.buildRustPackage {
        inherit pname version;
        src = pkgs.lib.cleanSource ../.;
        postPatch = ''
          substituteInPlace build.rs --replace-fail \
            'let sdk_dir = "/Library/Developer/CommandLineTools/SDKs";' \
            'let sdk_dir = "${pkgs.apple-sdk}/Platforms/MacOSX.platform/Developer/SDKs";'
        '';
        cargoLock.lockFile = ../Cargo.lock;
        buildInputs = [
          pkgs.apple-sdk.privateFrameworksHook
        ];

        # Do not run tests
        doCheck = false;

        meta = {
          # Tells `lib.getExe` which package name to get.
          mainProgram = pname;
          platforms = lib.platforms.darwin;
        };
      };
    };
}
