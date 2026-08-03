# Shared option schema and Lua-wrapping helpers for the paneru Home Manager and
# nix-darwin modules, which otherwise duplicate this verbatim and differ only in
# their platform-specific `config` block.
#
# Underscore-prefixed so flake.nix's `imports = [ (import-tree ./nix) ]` skips
# it: this is a plain module fragment imported by home.nix / darwin.nix, not a
# flake-parts module. Applied as `import ./_paneru-common.nix { inherit self; }`.
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.paneru;
  tomlFormat = pkgs.formats.toml { };

  luaPackages = cfg.lua.pkgs;
  resolvedExtraLuaPackages = if cfg.luaConfig.enable then cfg.extraLuaPackages luaPackages else [ ];
  luaPaths = lib.optional (resolvedExtraLuaPackages != [ ]) (
    lib.concatMapStringsSep ";" luaPackages.getLuaPath resolvedExtraLuaPackages
  );
  luaCPaths = lib.optional (resolvedExtraLuaPackages != [ ]) (
    lib.concatMapStringsSep ";" luaPackages.getLuaCPath resolvedExtraLuaPackages
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
  wrapPaneru =
    package:
    pkgs.symlinkJoin {
      name = "paneru-with-lua-wrapped";
      paths = [ package ];
      nativeBuildInputs = [ pkgs.makeWrapper ];
      passthru = package.passthru;
      postBuild = ''
        wrapProgram $out/bin/paneru ${lib.escapeShellArgs makeWrapperArgs}
      '';
      inherit (cfg.package) meta;
    };
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
      default = [ ];
      example = lib.literalExpression "[ pkgs.sketchybar ]";
      description = ''
        Extra packages to add to the PATH when paneru is run. This is
        useful for adding dependencies that paneru's Lua scripts may
        require, such as `sketchybar` for `require("sbar")`.
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
          scripting runtime (`init.lua`, `paneru.on`/`paneru.bind`,
          `paneru.setup`) compiled in — the `lua` Cargo feature. Disable for
          a build with no Lua dependency at all. Only takes effect when
          `package` is left at its default (an overrideable `paneru.override
          { enableLua = ...; }` derivation); implies `extraLuaPackages` and
          `config` are ignored when `false`.
        '';
      };
    };

    settings = lib.mkOption {
      type = lib.types.nullOr lib.types.attrs;
      default = null;
      description = ''
        Paneru configuration, rendered to a `paneru.toml`. Ignored for the
        options a `config` (`init.lua`) `paneru.setup{...}` call declares,
        which take precedence — see the paneru configuration guide.
      '';
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

    config = lib.mkOption {
      type = with lib.types; nullOr (either lines path);
      default = null;
      example = ''
        paneru.setup {
          options = { focus_follows_mouse = true },
          bindings = { ["window focus east"] = "alt - l" },
        }
      '';
      description = ''
        Contents of paneru's `init.lua` — either a block of Lua source or a
        path to a file — written to `$XDG_CONFIG_HOME/paneru/init.lua` (or
        `~/.paneru.lua` when XDG is disabled). Mirrors Home Manager's
        `services.sketchybar.config`.

        Requires `luaConfig.enable = true`. When the script calls
        `paneru.setup{...}` it becomes the authoritative configuration and the
        TOML `settings` are ignored; otherwise the two coexist (see the paneru
        configuration guide). Unlike sketchybar's config it is not marked
        executable — paneru loads it, it is not run as a shell script.
      '';
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

    # Computed, read-only: the generated config files the platform modules
    # write out (or reference via env vars), so neither has to repeat the
    # path-or-lines / TOML-generation logic.
    configFile = lib.mkOption {
      type = with lib.types; nullOr path;
      readOnly = true;
      default =
        if cfg.config == null then
          null
        else if lib.isPath cfg.config || lib.isStorePath cfg.config then
          cfg.config
        else
          pkgs.writeText "init.lua" cfg.config;
      description = "The generated `init.lua` file (from `config`), or `null`.";
    };

    settingsFile = lib.mkOption {
      type = with lib.types; nullOr path;
      readOnly = true;
      default = if cfg.settings == null then null else tomlFormat.generate "paneru.toml" cfg.settings;
      description = "The generated `paneru.toml` file (from `settings`), or `null`.";
    };
  };
}
