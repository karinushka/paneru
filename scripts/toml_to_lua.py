#!/usr/bin/env python3
import sys
import re

try:
    import tomllib
except ImportError:
    try:
        import tomli as tomllib
    except ImportError:
        tomllib = None


def parse_simple_toml_value(v):
    v = v.strip()
    if (v.startswith('"') and v.endswith('"')) or (v.startswith("'") and v.endswith("'")):
        return v[1:-1]
    if v == "true":
        return True
    if v == "false":
        return False
    if v.startswith('[') and v.endswith(']'):
        raw_items = v[1:-1].split(',')
        return [parse_simple_toml_value(item) for item in raw_items if item.strip()]
    try:
        if '.' in v:
            return float(v)
        return int(v)
    except ValueError:
        return v


def parse_simple_toml(text):
    data = {}
    current_section = []

    for line in text.splitlines():
        # Remove inline comments if not inside quotes
        comment_idx = line.find('#')
        if comment_idx != -1:
            # Check if # is inside quotes
            quotes = line[:comment_idx].count('"') + line[:comment_idx].count("'")
            if quotes % 2 == 0:
                line = line[:comment_idx]

        line = line.strip()
        if not line:
            continue

        m_sec = re.match(r'^\[([a-zA-Z0-9_\.]+)\]$', line)
        if m_sec:
            current_section = m_sec.group(1).split('.')
            continue

        if '=' in line:
            k, v = line.split('=', 1)
            k = k.strip()
            val = parse_simple_toml_value(v)

            target = data
            for sec in current_section:
                if sec not in target or not isinstance(target[sec], dict):
                    target[sec] = {}
                target = target[sec]
            target[k] = val

    return data


def format_lua_val(val, indent=0):
    if isinstance(val, bool):
        return "true" if val else "false"
    elif isinstance(val, (int, float)):
        return str(val)
    elif isinstance(val, str):
        escaped = val.replace('\\', '\\\\').replace('"', '\\"')
        return f'"{escaped}"'
    elif isinstance(val, list):
        items = ", ".join(format_lua_val(v, indent) for v in val)
        return f"{{ {items} }}"
    elif isinstance(val, dict):
        return format_lua_dict(val, indent)
    return str(val)


def format_lua_dict(d, indent=0, is_bindings=False):
    lines = []
    next_ind = "\t" * (indent + 1)

    for k, v in d.items():
        if is_bindings:
            lua_key = f'["{k.replace("_", " ")}"]'
        elif isinstance(k, str) and (k.isidentifier() and not k.startswith("_")):
            lua_key = k
        else:
            lua_key = f'["{k}"]'

        if isinstance(v, dict):
            child_is_bindings = k == "bindings"
            nested = format_lua_dict(v, indent + 1, child_is_bindings)
            lines.append(f"{next_ind}{lua_key} = {{\n{nested}\n{next_ind}}},")
        else:
            lines.append(f"{next_ind}{lua_key} = {format_lua_val(v, indent + 1)},")

    return "\n".join(lines)


def main():
    if len(sys.argv) > 1 and sys.argv[1] in ("-h", "--help"):
        print("Usage: python3 scripts/toml_to_lua.py [path/to/paneru.toml]")
        print("Converts a Paneru TOML configuration file to Lua syntax and prints to stdout.")
        sys.exit(0)

    filepath = sys.argv[1] if len(sys.argv) > 1 else "paneru.toml"

    try:
        if tomllib is not None:
            with open(filepath, "rb") as f:
                data = tomllib.load(f)
        else:
            with open(filepath, "r", encoding="utf-8") as f:
                data = parse_simple_toml(f.read())
    except Exception as e:
        sys.exit(f"Error reading TOML file '{filepath}': {e}")

    lua_body = format_lua_dict(data, indent=0)

    print("---@diagnostic disable: undefined-global\n")
    print("paneru.setup({")
    print(lua_body)
    print("})")


if __name__ == "__main__":
    main()
