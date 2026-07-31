{ lib, self, ... }:
{
  flake.darwinModules.paneru =
    { config, pkgs, ... }:
    let
      cfg = config.services.paneru;
      tomlFormat = pkgs.formats.toml { };

      luaPackages = cfg.lua.pkgs;
      luaPaths = lib.optional (resolvedExtraLuaPackages != [ ]) (
        lib.concatMapStringsSep ";" luaPackages.getLuaPath resolvedExtraLuaPackages
      );

      luaCPaths = (
        lib.optional (resolvedExtraLuaPackages != [ ]) (
          lib.concatMapStringsSep ";" luaPackages.getLuaCPath resolvedExtraLuaPackages
        )
      );
      makeWrapperArgs = lib.flatten (
        lib.filter (x: x != [ ]) [
          (lib.optional (cfg.extraPackages != [ ]) [
            "--prefix"
            "PATH"
            ":"
            "${lib.makeBinPath cfg.extraPackages}"
          ])

          (lib.optional (luaPaths != [ ]) [
            "--prefix"
            "LUA_PATH"
            ";"
            "${lib.concatStringsSep ";" luaPaths}"
          ])

          (lib.optional (luaCPaths != [ ]) [
            "--prefix"
            "LUA_CPATH"
            ";"
            "${lib.concatStringsSep ";" luaCPaths}"
          ])
        ]
      );
      resolvedExtraLuaPackages = if cfg.luaConfig.enable then cfg.extraLuaPackages luaPackages else [ ];
      luaPath = lib.concatMapStringsSep ";" luaPackages.getLuaPath resolvedExtraLuaPackages;
      wrapPaneru =
        package:
        pkgs.symlinkJoin {
          name = "paneru-with-lua-wrapped";
          paths = [ package ];
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postBuild = ''
            wrapProgram $out/bin/paneru ${lib.escapeShellArgs makeWrapperArgs}
          '';
          inherit (cfg.package) meta;
        };
      luaCPath = lib.concatMapStringsSep ";" luaPackages.getLuaCPath resolvedExtraLuaPackages;
    in
    {
      options.services.paneru = {
        enable = lib.mkEnableOption ''
          Install paneru and configure the launchd agent.

          The first time this is enabled after installing/updating, macOS will prompt you
          to grant accessibilty permissions item in System Settings.

          After granting permissions you may have to manually restart the service:
          `launchctl start com.github.karinushka.paneru`

          You can verify the service is running correctly from your terminal.
          Run: `launchctl list | grep paneru`

          In case of failure, check the logs with `cat /tmp/paneru.err.log`.
        '';

        package = lib.mkOption {
          type = lib.types.package;
          default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
          description = "The paneru package to use.";
        };
        extraPackages = lib.mkOption {
          type = with lib.types; listOf package;
          default = _: [ ];
          defaultText = lib.literalExpression "[ ]";
          description = ''
            Extra packages made available to paneru's path
          '';
        };
        finalPackage = lib.mkOption {
          type = lib.types.package;
          readOnly = true;
          default =
            if cfg.luaConfig.enable then
              wrapPaneru (
                cfg.package.override {
                  enableLua = true;
                  lua = cfg.lua;
                }
              )
            else
              cfg.package.override { enableLua = false; };

          description = ''
            The final paneru package that will be installed and run. This is
            the result of `package.override { enableLua = ...; lua = ...; }`
            (see `luaConfig.enable` and `lua`), so it may differ from
            `package` if those options are set.
          '';
        };

        lua = lib.mkOption {
          type = lib.types.package;
          default = cfg.package.luaModule.lua;
          defaultText = lib.literalExpression "config.services.paneru.package.luaModule.lua";
          description = ''
            The Lua interpreter `extraLuaPackages` are resolved against.
            Defaults to whichever interpreter `services.paneru.package`'s
            loadable Lua module was built for (see `paneru.luaModule.override`
            in `nix/package.nix`), so overriding `package` alone keeps this in
            sync; override this directly if you need `extraLuaPackages` to
            resolve against a different interpreter than `package` was built
            with.
          '';
        };

        luaConfig = {
          enable = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = ''
              Whether `services.paneru.package` is built with the embedded Lua
              scripting runtime (`init.lua`, `paneru.on`/`paneru.bind`)
              compiled in — the `lua` Cargo feature. Disable for a build with
              no Lua dependency at all. Only takes effect when `package` is
              left at its default (an overrideable `paneru.override { enableLua
              = ...; }` derivation); implies `extraLuaPackages` is ignored when
              `false`.
            '';
          };
        };

        settings = lib.mkOption {
          type = lib.types.nullOr lib.types.attrs;
          default = null;
          description = "Paneru configuration";
          example = {
            options = {
              focus_follows_mouse = true;
              mouse_follows_focus = true;
            };
            bindings = {
              window_focus_west = "cmd - h";
              window_focus_east = "cmd - l";
              window_resize = "alt - r";
              window_center = "alt - c";
              quit = "ctrl + alt - q";
            };
          };
        };

        extraLuaPackages = lib.mkOption {
          type = with lib.types; functionTo (listOf package);
          default = _: [ ];
          defaultText = lib.literalExpression "luaPs: [ ]";
          example = lib.literalExpression "luaPs: [ (luaPs.callPackage ./sbarlua.nix { }) ]";
          description = ''
            Extra Lua packages made available to paneru's embedded Lua runtime
            via `require(...)` (e.g. `require("sbar")` to call SketchyBar's
            Lua bridge directly from an `init.lua` `paneru.on(...)` handler).
            This option accepts a function that takes a Lua package set and
            returns the packages to expose; it is deliberately the same shape
            as Home Manager's `programs.sketchybar.extraLuaPackages`, so the
            same package (e.g. `sbarlua`) can be passed to both options
            without duplicating the derivation.
          '';
        };
      };

      config = lib.mkIf cfg.enable {
        environment.systemPackages = [ cfg.finalPackage ];
        # TODO: Once nix-darwin supports it, prefer `launchd.agents.paneru` so `system.primaryUser` is not needed.
        # See <https://github.com/nix-darwin/nix-darwin/issues/1255>
        launchd.user.agents.paneru = {
          serviceConfig = {
            Label = "com.github.karinushka.paneru";
            KeepAlive = {
              Crashed = true;
              SuccessfulExit = false;
            };
            Nice = -20;
            ProcessType = "Interactive";
            EnvironmentVariables = {
              PANERU_CONFIG = lib.mkIf (cfg.settings != null) (
                toString (tomlFormat.generate "paneru.toml" cfg.settings)
              );
              NO_COLOR = "1";
            };
            RunAtLoad = true;
            StandardOutPath = "/tmp/paneru.log";
            StandardErrorPath = "/tmp/paneru.err.log";
            Program = lib.getExe cfg.finalPackage;
          };
        };
      };
    };
}
