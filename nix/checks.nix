{
  lib,
  inputs,
  self,
  ...
}:
{
  perSystem =
    { pkgs, system, ... }:
    let
      buildFromConfig =
        configuration: sel:
        sel
          (import inputs.nix-darwin {
            inherit configuration system;
            nixpkgs = inputs.nixpkgs;
          }).config;

      makeTest =
        name: test:
        let
          configuration =
            {
              config,
              lib,
              pkgs,
              ...
            }:
            {
              imports = [
                self.darwinModules.paneru
                test
              ];

              options = {
                out = lib.mkOption {
                  type = lib.types.package;
                };
                test = lib.mkOption {
                  type = lib.types.lines;
                  default = "";
                };
              };

              config = {
                out = config.system.build.toplevel;
                system.stateVersion = lib.mkDefault config.system.maxStateVersion;
                system.build.run-test =
                  pkgs.runCommand "darwin-test-${name}"
                    {
                      allowSubstitutes = false;
                      preferLocalBuild = true;
                    }
                    ''
                      #! ${pkgs.stdenv.shell}
                      set -e

                      echo >&2 "running tests for system ${config.out}"
                      echo >&2
                      ${config.test}
                      echo >&2 ok
                      touch $out
                    '';
              };
            };
        in
        buildFromConfig configuration (config: config.system.build.run-test);
    in
    {
      checks.darwin-module = makeTest "darwin-module" (
        { config, pkgs, ... }:
        let
          plistPath = "${config.out}/user/Library/LaunchAgents/com.github.karinushka.paneru.plist";
        in
        {
          system.primaryUser = "test-paneru-user";
          services.paneru = {
            enable = true;
            settings = {
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

          test = # sh
            ''
              PATH=${
                lib.makeBinPath [
                  pkgs.jq
                  pkgs.toml2json
                  pkgs.xcbuild
                ]
              }:$PATH

              echo >&2 "checking paneru service in ~/Library/LaunchAgents"
              plutil -lint ${plistPath}
              plutil -convert json ${plistPath} -o service.json
              <service.json jq -e ".EnvironmentVariables.NO_COLOR == \"1\""
              <service.json jq -e ".KeepAlive.Crashed == true"
              <service.json jq -e ".KeepAlive.SuccessfulExit == false"
              <service.json jq -e ".Label == \"com.github.karinushka.paneru\""
              <service.json jq -e ".ProcessType == \"Interactive\""
              <service.json jq -e ".RunAtLoad == true"
              <service.json jq -e ".StandardErrorPath == \"/tmp/paneru.err.log\""
              <service.json jq -e ".StandardOutPath == \"/tmp/paneru.log\""

              confPath=`<service.json jq -r ".EnvironmentVariables.PANERU_CONFIG"`
              echo >&2 "checking config in $confPath"
              conf=`<"$confPath" toml2json`
              echo $conf | jq -e ".options.focus_follows_mouse == true"
              echo $conf | jq -e ".options.mouse_follows_focus == true"
            '';
        }
      );
    };
}
