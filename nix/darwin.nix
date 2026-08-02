{ lib, self, ... }:
{
  flake.darwinModules.paneru =
    { config, pkgs, ... }:
    let
      cfg = config.services.paneru;
    in
    {
      imports = [ (import ./_paneru-common.nix { inherit self; }) ];

      config = lib.mkIf cfg.enable {
        assertions = [
          {
            assertion = cfg.config == null || cfg.luaConfig.enable;
            message = "services.paneru.config (init.lua) requires services.paneru.luaConfig.enable = true.";
          }
        ];
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
              # The paneru.setup{...} in PANERU_LUA (init.lua) takes precedence
              # over the options in PANERU_CONFIG (paneru.toml).
              PANERU_CONFIG = lib.mkIf (cfg.settings != null) (toString cfg.settingsFile);
              PANERU_LUA = lib.mkIf (cfg.config != null) (toString cfg.configFile);
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
