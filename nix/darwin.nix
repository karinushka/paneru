{ lib, self, ... }:
{
  flake.darwinModules.paneru =
    { config, pkgs, ... }:
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
          `launchctl start paneru`

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
          description = "Configuration to put in `/etc/paneru.toml`.";
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
        environment.systemPackages = [ cfg.package ];
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
            Program = lib.getExe cfg.package;
          };
        };
      };
    };
}
