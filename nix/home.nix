{ self, ... }:
{
  flake.homeModules.paneru =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      cfg = config.services.paneru;
    in
    {
      imports = [ (import ./_paneru-common.nix { inherit self; }) ];

      config = lib.mkIf cfg.enable {
        assertions = [
          (lib.hm.assertions.assertPlatform "services.paneru" pkgs lib.platforms.darwin)
          {
            assertion = cfg.config == null || cfg.luaConfig.enable;
            message = "services.paneru.config (init.lua) requires services.paneru.luaConfig.enable = true.";
          }
        ];
        home.packages = [ cfg.finalPackage ];
        launchd.agents.paneru = {
          enable = true;
          config = {
            Label = "com.github.karinushka.paneru";
            # The Mach service clients look up. launchd creates and holds the
            # port, so `paneru send-cmd`/`query`/`subscribe` and the Lua module
            # keep working across a daemon restart rather than racing it to
            # register the name.
            MachServices = {
              "com.github.karinushka.paneru" = true;
            };
            KeepAlive = {
              Crashed = true;
              SuccessfulExit = false;
            };
            Nice = -20;
            ProcessType = "Interactive";
            EnvironmentVariables = {
              NO_COLOR = "1";
              XDG_CONFIG_HOME =
                if config.xdg.enable then config.xdg.configHome else "${config.home.homeDirectory}/.config";
            };
            RunAtLoad = true;
            StandardOutPath = "/tmp/paneru.log";
            StandardErrorPath = "/tmp/paneru.err.log";
            Program = lib.getExe cfg.finalPackage;
          };
        };

        # TOML config (paneru.toml). The paneru.setup{...} in `config` (init.lua)
        # takes precedence over the options declared here.
        xdg.configFile."paneru/paneru.toml" = lib.mkIf (config.xdg.enable && cfg.settings != null) {
          source = cfg.settingsFile;
        };
        home.file.".paneru.toml" = lib.mkIf (!config.xdg.enable && cfg.settings != null) {
          source = cfg.settingsFile;
        };

        # Lua config (init.lua), following paneru's discovery order:
        # $XDG_CONFIG_HOME/paneru/init.lua, else ~/.paneru.lua.
        xdg.configFile."paneru/init.lua" = lib.mkIf (config.xdg.enable && cfg.config != null) {
          source = cfg.configFile;
        };
        home.file.".paneru.lua" = lib.mkIf (!config.xdg.enable && cfg.config != null) {
          source = cfg.configFile;
        };
      };
    };
}
