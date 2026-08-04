{ self, inputs, ... }:
{
  perSystem =
    {
      lib,
      pkgs,
      self',
      config,
      ...
    }:
    let
      craneLib = inputs.crane.mkLib pkgs;

      mkDate =
        longDate:
        (lib.concatStringsSep "-" [
          (builtins.substring 0 4 longDate)
          (builtins.substring 4 2 longDate)
          (builtins.substring 6 2 longDate)
        ]);
      props = builtins.fromTOML (builtins.readFile ../Cargo.toml);
      pname = "paneru";
      version = props.package.version;

      # One source tree for every derivation here: the daemon and the loadable
      # Lua module are members of the same workspace sharing one lock file, so
      # they vendor identical dependencies. `commonCargoSources` keeps only
      # Rust/Cargo files, so the plists pulled in by `embed_plist!` /
      # `include_str!` are added back.
      paneruSource = lib.fileset.toSource {
        root = ../.;
        fileset = lib.fileset.unions [
          (craneLib.fileset.commonCargoSources ../.)
          (lib.fileset.fileFilter (file: file.hasExt "plist") ../.)
        ];
      };

      # The Cargo ABI feature naming an interpreter (`luajit`, `lua54`, ...).
      # Pure, so it is the one Lua-related thing that can live out here.
      #
      # `lua51` is rejected here rather than passed through: the workspace no
      # longer offers that feature (the async world-access calls need an
      # interpreter that can yield out of a Rust callback, which reference 5.1
      # cannot), and an unknown `--features` reaches the user as a cargo error
      # naming a feature they never asked for. LuaJIT reports luaversion "5.1"
      # but is matched by name before this, and can yield.
      luaFeature =
        lua:
        if lib.hasInfix "luajit" (lua.pname or lua.name or "") then
          "luajit"
        else
          let
            feature = "lua" + lib.replaceStrings [ "." ] [ "" ] lua.luaversion;
          in
          if feature == "lua51" then
            throw "paneru: Lua 5.1 is not supported (its C API cannot yield out of a Rust callback); use luajit or Lua 5.2+."
          else
            feature;

      # Arguments common to everything built from this workspace.
      commonArgs = {
        inherit version;
        src = paneruSource;
        # The daemon links AppKit/etc.; nothing here runs tests as part of the
        # build.
        doCheck = false;
      };

      # The interpreter the dependency artifacts are built against. Also the
      # default for `mkPaneru`'s `lua` knob: LuaJIT's tracing JIT keeps handler
      # dispatch cheap enough to run on the hot event path.
      defaultLua = pkgs.luajit;

      # Third-party dependencies, compiled once and reused by every derivation
      # here instead of being rebuilt per source change.
      #
      # Deliberately NOT a function of `mkPaneru`'s knobs. Parameterising it
      # gives one `paneru-deps` per (enableLua, lua) combination — same name,
      # different store path — so a config that touches both a Lua and a non-Lua
      # variant (this flake's own `paneru` and `paneru-lua`, say) builds the
      # whole dependency graph twice over. Building the widest feature set once
      # keeps a single `paneru-deps` in the closure: a non-Lua build simply
      # ignores the `mlua`/`mlua-sys` artifacts, and a build against a different
      # interpreter recompiles only those two crates inside its own derivation
      # rather than the graph beneath them.
      sharedDeps = craneLib.buildDepsOnly (
        commonArgs
        // {
          inherit pname;
          cargoExtraArgs = "--features lua,${luaFeature defaultLua}";
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [
            pkgs.apple-sdk.privateFrameworksHook
            #            ##            (pkgs.darwinMinVersionHook "12.5")
            defaultLua
          ];
        }
      );

      # Everything else is a function of the two knobs.
      #
      # Overrideable like a nixpkgs package (`paneru.override { enableLua =
      # true; lua = pkgs.lua5_4; }`). `enableLua` toggles the `lua` Cargo
      # feature, which builds in the `init.lua` scripting runtime
      # (`paneru.on`/`paneru.bind`); it is off by default at the Cargo level
      # (see `Cargo.toml`) and here. `lua` resolves the whole Lua dependency
      # graph: the daemon's `mlua` ABI feature (`luajit`/`lua54`/..., via
      # `luaFeature`), the interpreter it links against, and the one the
      # loadable module (`paneru.luaModule`) is built for. It defaults to
      # `defaultLua`, the interpreter `sharedDeps` is built against, and the
      # module stays independently overrideable afterwards via
      # `paneru.luaModule.override { lua = ...; }`.
      mkPaneru =
        {
          enableLua ? false,
          lua ? defaultLua,
        }:
        let
          # What building against an interpreter takes: pkg-config to find it,
          # the interpreter itself for its headers and library. Deliberately
          # *not* mlua's `vendored` feature, which builds an interpreter from
          # source and fails here — `luajit-src` copies its sources out of the
          # read-only store and then cannot write to the copy.
          luaBuildInputs = {
            nativeBuildInputs = lib.optional enableLua pkgs.pkg-config;
            buildInputs = lib.optional enableLua lua;
          };

          cargoExtraArgs = lib.optionalString enableLua "--features lua,${luaFeature lua}";

          # --- Loadable Lua C module, as a function of the interpreter -------
          #
          # Mirrors a nixpkgs Lua package: takes `lua`, defaults to the one the
          # daemon was built for, and is overrideable
          # (`paneru.luaModule.override { lua = pkgs.lua5_4; }`). Unlike the
          # daemon it links no Lua at all, resolving `lua_*` from the host
          # interpreter at load time (see the crate's build.rs); the
          # interpreter only supplies headers. Not exposed as its own top-level
          # flake package — it only makes sense in the context of a `paneru`
          # build, so it is reached through `paneru.luaModule`.
          mkLuaModule =
            {
              lua ? pkgs.luajit,
            }:
            let
              ver = lua.luaversion;
              drv = craneLib.buildPackage (
                commonArgs
                // {
                  inherit version;
                  pname = "lua${ver}-paneru";
                  # The module is a workspace member, just not a default one
                  # (see the root Cargo.toml), so it builds from the same
                  # source and dependency artifacts as the daemon.
                  cargoArtifacts = sharedDeps;
                  nativeBuildInputs = [ pkgs.pkg-config ];
                  buildInputs = [ lua ];
                  # Select the Lua ABI feature matching the interpreter.
                  cargoExtraArgs = "-p paneru-lua --no-default-features --features module,${luaFeature lua}";
                  # Install the cdylib as `paneru.so` under the interpreter's
                  # C-module path, so a `${moduleDir}/?.so` cpath entry finds it.
                  installPhaseCommand = ''
                    so=$(find target -name 'libpaneru_lua.dylib' -print -quit)
                    if [ -z "$so" ]; then
                      echo "paneru-lua: could not find built cdylib" >&2
                      exit 1
                    fi
                    install -Dm555 "$so" "$out/lib/lua/${ver}/paneru.so"
                  '';
                  meta = {
                    description = "Loadable Lua module for the Paneru window manager";
                    platforms = lib.platforms.darwin;
                  };
                }
              );
            in
            drv.overrideAttrs (old: {
              passthru = (old.passthru or { }) // {
                inherit lua;
                moduleDir = "${drv}/lib/lua/${ver}";
                modulePath = "${drv}/lib/lua/${ver}/paneru.so";
              };
            });
        in
        craneLib.buildPackage (
          commonArgs
          // luaBuildInputs
          // {
            inherit cargoExtraArgs;
            pname = "paneru${if enableLua then "-with-lua" else ""}";
            cargoArtifacts = sharedDeps;

            # Expose the loadable Lua module so downstream configs can
            # reference it as `paneru.luaModule` (the derivation, built for
            # the same interpreter as `lua` above), `paneru.luaModule.moduleDir`
            # (the dir for a `package.cpath` `?.so` entry), or
            # `paneru.luaModule.modulePath` (the `paneru.so` file).
            passthru.luaModule = lib.makeOverridable mkLuaModule { inherit lua; };

            meta = {
              # Tells `lib.getExe` which package name to get.
              mainProgram = pname;
              platforms = lib.platforms.darwin;
            };
          }
        );
    in
    {
      devShells.default = craneLib.devShell {
        inputsFrom = [
          self'.packages.paneru
          self'.packages.paneru.luaModule
        ];
        packages = [
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.cargo-flamegraph
          pkgs.clippy
          pkgs.pkg-config
        ];
      };

      packages.default = self'.packages.paneru;

      packages.paneru = lib.makeOverridable mkPaneru { };
      packages.paneru-lua = self'.packages.paneru.override { enableLua = true; };
    };
}
