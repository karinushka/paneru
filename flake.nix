{
  description = "A sliding, tiling window manager for MacOS";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      pkgs = import nixpkgs { system = "aarch64-darwin"; };
      package = pkgs.rustPlatform.buildRustPackage {
        pname = "paneru";
        version = "0.3.0";
        src = pkgs.lib.cleanSource ./.;
        postPatch = ''
          substituteInPlace build.rs --replace-fail \
            'let sdk_dir = "/Library/Developer/CommandLineTools/SDKs";' \
            'let sdk_dir = "${pkgs.apple-sdk}/Platforms/MacOSX.platform/Developer/SDKs";'
        '';
        cargoLock.lockFile = ./Cargo.lock;
        buildInputs = [
          pkgs.apple-sdk.privateFrameworksHook
        ];
      };
    in
    {
      packages.aarch64-darwin.default = self.packages.aarch64-darwin.paneru;
      packages.aarch64-darwin.paneru = package;
      homeModules.paneru =
        { config, lib, ... }:
        let
          cfg = config.services.paneru;
          tomlFormat = pkgs.formats.toml { };
        in
        {
          options.services.paneru = {
            enable = lib.mkEnableOption ''
              Install paneru and configure the launchd agent.

              The first time this is enabled, macOS will prompt you to allow this background
              item in System Settings.

              You can verify the service is running correctly from your terminal.
              Run: `launchctl list | grep paneru`

              In case of failure, check the logs with `cat /tmp/paneru.err.log`.
            '';

            package = lib.mkOption {
              type = lib.types.package;
              default = package;
              description = "The paneru package to use.";
            };

            settings = lib.mkOption {
              type = lib.types.attrs;
              default = { };
              description = "Configuration to put in `~/.paneru.toml`.";
              example = {
                options = {
                  focus_follows_mouse = true;
                  preset_column_widths = [
                    0.25
                    0.33
                    0.5
                    0.66
                    0.75
                  ];
                  swipe_gesture_fingers = 4;
                  animation_speed = 4000;
                };
                bindings = {
                  window_focus_west = "cmd - h";
                  window_focus_east = "cmd - l";
                  window_focus_north = "cmd - k";
                  window_focus_south = "cmd - j";
                  window_swap_west = "alt - h";
                  window_swap_east = "alt - l";
                  window_swap_first = "alt + shift - h";
                  window_swap_last = "alt + shift - l";
                  window_center = "alt - c";
                  window_resize = "alt - r";
                  window_manage = "ctrl + alt - t";
                  window_stack = "alt - ]";
                  window_unstack = "alt + shift - ]";
                  quit = "ctrl + alt - q";
                };
              };
            };
          };

          config = lib.mkIf cfg.enable {
            assertions = [ (lib.hm.assertions.assertPlatform "services.paneru" pkgs lib.platforms.darwin) ];
            launchd.agents.paneru = {
              enable = true;
              config = {
                KeepAlive = {
                  Crashed = true;
                  SuccessfulExit = false;
                };
                Label = "Paneru";
                Nice = -20;
                ProcessType = "Interactive";
                EnvironmentVariables = {
                  NO_COLOR = "1";
                };
                RunAtLoad = true;
                StandardOutPath = "/tmp/paneru.log";
                StandardErrorPath = "/tmp/paneru.err.log";
                Program = cfg.package + /bin/paneru;
              };
            };

            xdg.configFile."paneru/paneru.toml".source =
              tomlFormat.generate "paneru.toml" config.services.paneru.settings;
          };
        };
    };
}
