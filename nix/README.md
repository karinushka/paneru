# Paneru Nix Flake

Add the paneru flake to your inputs:

```nix
# flake.nix
inputs.paneru = {
  url = "github:karinushka/paneru";
  inputs.nixpkgs.follows = "nixpkgs";
}
```

## Usage

- [nix-darwin](#nix-darwin-or-home-manager)
- [Home Manager](#nix-darwin-or-home-manager)
- [Standalone](#standalone)

### [nix-darwin](https://github.com/nix-darwin/nix-darwin) or [Home Manager](https://github.com/nix-community/home-manager)

Instead of manually installing and configuring paneru, you can use either
nix-darwin or home-manager to configure paneru and setup a launchd agent entirely with nix.

Which one you use is entirely up to preference, but **do not** use both at the same time.

If you are unsure which one to use, prefer using the darwin module.

#### Options

Both the nix-darwin module (`darwinModules.paneru`) and Home Manager module (`homeModules.paneru`)
expose the same following options:

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `services.paneru.enable` | `boolean` | `false` | Generate and enable the launchd agent (`~/Library/LaunchAgents/com.github.karinushka.paneru.plist`) |
| `services.paneru.package` | `package` | `self.packages.<system>.paneru` | Package to use |
| `services.paneru.settings` | `null` or `attribute set` | `null` | Paneru configuration (See [`CONFIGURATION.md`](/CONFIGURATION.md)) |

#### Example

```nix
# configuration.nix (nix-darwin) or
# home.nix (Home Manager)
{ inputs, ... }:

{
  imports = [
    inputs.paneru.darwinModules.paneru # nix-darwin
    inputs.paneru.homeModules.paneru # home-manager
  ];

  services.paneru = {
    enable = true;
    # Paneru configuration
    # See CONFIGURATION.md for a list of all options
    settings = {
      options = {
        focus_follows_mouse = true;
        mouse_follows_focus = true;
        preset_column_widths = [0.25 0.33 0.5 0.66 0.75];
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
}
```

> [!NOTE]
> After installing/updating paneru, macOS will prompt you to grant accessibilty permissions item in System Settings.
> You may have to manually restart the paneru service using `launchctl`:
>
> ```shell
> launchctl start com.github.karinushka.paneru
> ```

### Standalone

This flake provides the following packages:

- `packages.<system>.paneru`
- `packages.<system>.default` *(alias for `packages.<system>.paneru`)*

#### Usage

##### Install

```nix
{ inputs, ... }:

{
  # nix-darwin configuration (configuration.nix)
  # system-wide
  environment.systemPackages = [ inputs.paneru.packages.${pkgs.system}.paneru ]
  # or per-user
  users.users."<name>".packages = [ inputs.paneru.packages.${pkgs.system}.paneru ]
}
```

##### Run without installing

> [!NOTE]
> Running paneru requires a configuration to be present (See [`CONFIGURATION.md`](/CONFIGURATION.md))

```shell
nix run github:karinushka/paneru
```

## Troubleshooting

Here are some tips for debugging when using either the nix-darwin or home-manager module.

**1. Check if the launchd agent exists**

```shell
launchctl list | grep paneru
```

```
PID     Status  Label
12345   0       com.github.karinushka.paneru
```

**2. Check the logs**

Logs can be found at `/tmp/paneru.log` and `/tmp/paneru.err.log`.

**3. Try manually starting the launchd agent**

```shell
launchctl start com.github.karinushka.paneru
```
