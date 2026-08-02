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
- [Other](#other)

### [nix-darwin](https://github.com/nix-darwin/nix-darwin) or [Home Manager](https://github.com/nix-community/home-manager)

Instead of manually installing and configuring paneru, you can use either
nix-darwin or Home Manager to configure paneru and setup a launchd agent entirely with nix.

Which one you use is entirely up to preference, but **do not** use both at the same time.

If you are unsure which one to use, prefer using the darwin module.

#### Options

Both the nix-darwin module (`darwinModules.paneru`) and Home Manager module (`homeModules.paneru`)
expose the same following options:

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `services.paneru.enable` | `boolean` | `false` | Generate and enable the launchd agent |
| `services.paneru.package` | `package` | `self.packages.<system>.paneru` | Package to use |
| `services.paneru.config` | `null`, lines, or path | `null` | Paneru's `init.lua` (Lua source or a file). Written to `$XDG_CONFIG_HOME/paneru/init.lua` (or `~/.paneru.lua`). Mirrors Home Manager's `services.sketchybar.config`. A `paneru.setup{...}` here takes precedence over `settings`. Requires `luaConfig.enable`. |
| `services.paneru.settings` | `null` or `attribute set` | `null` | Paneru TOML configuration (See [`CONFIGURATION.md`](/CONFIGURATION.md)) |
| `services.paneru.extraPackages` | `list of package` | `[ ]` | Extra packages on paneru's `PATH` at runtime (e.g. `sketchybar`). |
| `services.paneru.luaConfig.enable` | `boolean` | `true` | Whether `package` is built with the embedded Lua scripting runtime (`init.lua`) compiled in. Only takes effect when `package` is left at its default. |
| `services.paneru.lua` | `package` | `package.luaModule.lua` | The Lua interpreter `extraLuaPackages` are resolved against. |
| `services.paneru.extraLuaPackages` | `function` | `luaPs: [ ]` | Extra Lua packages available to `init.lua` via `require(...)` (e.g. [sbarlua](https://github.com/FelixKratz/SbarLua)). Same shape as Home Manager's `programs.sketchybar.extraLuaPackages` — a function from a Lua package set to a list of derivations. |

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
        window_balance = "alt - b";
        quit = "ctrl + alt - q";
      };
    };
  };
}
```

Alternatively, configure paneru entirely from Lua with `config` (the `init.lua`),
the same way Home Manager's `services.sketchybar.config` works. A `paneru.setup{...}`
call is authoritative and makes `settings` unnecessary — see
[`CONFIGURATION.md`](/CONFIGURATION.md#configuration-from-lua):

```nix
services.paneru = {
  enable = true;
  config = ''
    paneru.setup {
      options = { focus_follows_mouse = true, mouse_follows_focus = true },
      bindings = {
        ["window focus west"] = "cmd - h",
        ["window focus east"] = "cmd - l",
        ["quit"] = "ctrl + alt - q",
      },
    }

    paneru.bind("alt - j", function(state) paneru.run("window focus south") end)
  '';
};
```

To make extra Lua modules available to `init.lua` (e.g. to call SketchyBar's
Lua bridge directly from a `paneru.on` handler, see
[`CONFIGURATION.md`](/CONFIGURATION.md#8-lua-scripting)):

```nix
services.paneru.extraLuaPackages = luaPs: [ (luaPs.callPackage ./sbarlua.nix { }) ];
```

> [!NOTE]
> After installing/updating paneru, macOS will prompt you to grant accessibility permissions in System Settings.
> You may have to manually restart the paneru service using `launchctl`:
>
> ```shell
> launchctl start com.github.karinushka.paneru
> ```

### Other

If neither nix-darwin nor Home Manager suits your use case, the flake provides the following packages:

- `packages.<system>.paneru`
- `packages.<system>.default` *(alias for `packages.<system>.paneru`)*

#### Run without installing

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
