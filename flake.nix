{
  description = "A sliding, tiling window manager for MacOS";

  inputs = {
    flake-parts.url = "github:hercules-ci/flake-parts";
    import-tree.url = "github:vic/import-tree";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
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
          { config, pkgs, ... }:

          {

            # Run `nix fmt .` to format all nix files in the repo.
            # `nixfmt-tree` allows passing a directory to format all files within it.
            formatter = pkgs.nixfmt-tree;

            # Allows running `nix develop` to get a shell with `paneru` and rust build dependencies available.
            # Uses `mkShell` (not `mkShellNoCC`) so a C compiler is present: the
            # daemon's `mlua` builds Lua from vendored C sources, and the
            # `crates/lua` module compiles against Lua headers (via pkg-config).
          };
      }
    );
}
