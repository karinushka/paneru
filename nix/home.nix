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
      tomlFormat = pkgs.formats.toml { };
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
      };

      config = lib.mkIf cfg.enable {
        assertions = [ (lib.hm.assertions.assertPlatform "services.paneru" pkgs lib.platforms.darwin) ];
        home.packages = [ cfg.package ];
        launchd.agents.paneru = {
          enable = true;
          config = {
            Label = "com.github.karinushka.paneru";
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
            Program = lib.getExe cfg.package;
          };
        };

        xdg.configFile."paneru/paneru.toml" = lib.mkIf (config.xdg.enable && cfg.settings != null) {
          source = tomlFormat.generate "paneru.toml" cfg.settings;
        };

        home.file.".paneru.toml" = lib.mkIf (!config.xdg.enable && cfg.settings != null) {
          source = tomlFormat.generate ".paneru.toml" cfg.settings;
        };
      };
    };
}
