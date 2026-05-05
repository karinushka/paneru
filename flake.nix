{
  description = "A sliding, tiling window manager for MacOS";

  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    import-tree.url = "github:vic/import-tree";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nix-darwin = {
      url = "github:nix-darwin/nix-darwin/master";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } (
      { lib, self, ... }:
      {
        imports = [ (inputs.import-tree ./nix) ];
        systems = lib.platforms.darwin;
        flake = {
          overlays.default = final: prev: {
            paneru = self.packages.aarch64-darwin.default;
          };
        };
        perSystem =
          { pkgs, ... }:
          {
            # Run `nix fmt .` to format all nix files in the repo.
            # `nixfmt-tree` allows passing a directory to format all files within it.
            formatter = pkgs.nixfmt-tree;

            # Allows running `nix develop` to get a shell with `paneru` and rust build dependencies available.
            devShells.default = pkgs.mkShellNoCC {
              packages = [
                self.packages.aarch64-darwin.default
                pkgs.rustc
                pkgs.cargo
                pkgs.rustfmt
                pkgs.clippy
              ];
            };
          };
      }
    );
}
