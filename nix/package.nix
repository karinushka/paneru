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

      # Arguments common to everything built from this workspace.
      commonArgs = {
        inherit version;
        src = paneruSource;
        # The daemon links AppKit/etc. and embeds a vendored Lua (built from C);
        # nothing here runs tests as part of the build.
        doCheck = false;
      };

      # Third-party dependencies, compiled once and reused by both packages (and
      # any future crane checks) instead of being rebuilt per source change.
      sharedDeps = craneLib.buildDepsOnly (
        commonArgs
        // {
          inherit pname;
          buildInputs = [
            pkgs.apple-sdk.privateFrameworksHook
            #            ##            (pkgs.darwinMinVersionHook "12.5")
          ];
        }
      );

      # --- Daemon (workspace root), built with crane ------------------------
      daemonArgs = commonArgs // {
        inherit pname;

        inherit version;
        cargoArtifacts = sharedDeps;

      };

      # --- Loadable Lua C module, as a function of the interpreter ----------
      #
      # Mirrors a nixpkgs Lua package: takes `lua`, defaults to LuaJIT, and is
      # overrideable (`paneru.luaModule.override { lua = pkgs.lua5_4; }`). The
      # module links no Lua and resolves `lua_*` from the host interpreter at
      # load time (see the crate's build.rs); the interpreter only supplies
      # headers. Not exposed as its own top-level flake package — it only
      # makes sense in the context of a `paneru` build, so it's reached via
      # `paneru.luaModule` (`mkPaneru`, below).
      luaFeature =
        lua:
        if lib.hasInfix "luajit" (lua.pname or lua.name or "") then
          "luajit"
        else
          "lua" + lib.replaceStrings [ "." ] [ "" ] lua.luaversion;

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
              # The module is a workspace member, just not a default one (see
              # the root Cargo.toml), so it builds from the same source and
              # dependency artifacts as the daemon.
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

      # Overrideable like a nixpkgs package (`paneru.override { enableLua =
      # false; lua = pkgs.lua5_4; }`): `enableLua` toggles the `lua` Cargo
      # feature, which builds in the `init.lua` scripting runtime
      # (`paneru.on`/`paneru.bind`). Off by default at the Cargo level (see
      # `Cargo.toml`); on by default here since that's the expected experience
      # for the flake's own package. `lua` resolves the whole Lua dependency
      # graph: it both picks the daemon's own vendored `mlua` ABI feature
      # (`luajit`/`lua54`/..., matching `${luaFeature lua}`) and the
      # interpreter the loadable Lua module (`paneru.luaModule`) is built for
      # — defaults to LuaJIT, whose tracing JIT keeps handler dispatch cheap
      # enough to run on the hot event path — and is independently
      # overrideable afterwards via `paneru.luaModule.override { lua = ...; }`.
      mkPaneru =
        {
          enableLua ? false,
          lua ? pkgs.luajit,
        }:
        craneLib.buildPackage (
          daemonArgs
          // {
            pname = "paneru${if enableLua then "-with-lua" else ""}";
            cargoExtraArgs = lib.optionalString enableLua "--features lua,vendored,${luaFeature lua}";

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
          pkgs.clippy
          pkgs.pkg-config
        ];
      };

      packages.default = self'.packages.paneru;

      packages.paneru = lib.makeOverridable mkPaneru { };
      packages.paneru-lua = self'.packages.paneru.override { enableLua = true; };
    };
}
