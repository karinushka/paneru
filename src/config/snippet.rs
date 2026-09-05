//! Generates a ready-to-paste `[windows]` rule for a window.
//!
//! Only two fields in a window rule are matchers: `title` (a regex, matched
//! unanchored) and `bundle_id` (exact string equality). See
//! [`crate::config::Config::find_window_properties`]. Everything else a rule can
//! carry is an effect, so the snippet emits the two matchers live and leaves the
//! rest — plus the window's identity, which no matcher can use — as comments.

use bevy::ecs::resource::Resource;

/// Which configuration language the snippet is written in. A Lua `init.lua`
/// disables the TOML file entirely (see [`crate::config::CONFIGURATION_FILE`]),
/// so the snippet has to follow whichever one is in charge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Resource)]
pub enum SnippetDialect {
    #[default]
    Toml,
    Lua,
}

/// The window a rule is being written for.
#[derive(Clone, Copy, Debug)]
pub struct RuleSubject<'a> {
    pub app_name: &'a str,
    pub bundle_id: &'a str,
    pub title: &'a str,
    pub role: &'a str,
    pub subrole: &'a str,
}

/// Builds the snippet for `subject` in `dialect`.
#[must_use]
pub fn window_rule_snippet(dialect: SnippetDialect, subject: &RuleSubject<'_>) -> String {
    let key = rule_key(subject.app_name);
    let title = title_pattern(subject.title);
    // An anchored empty title matches nothing useful, so the wildcard is already
    // the live pattern and there is no alternative left to suggest.
    let wildcard_alternative = !subject.title.is_empty();
    let identity = identity_comment(subject);
    let bundle_id = quote(subject.bundle_id);

    let mut lines = Vec::new();
    match dialect {
        SnippetDialect::Toml => {
            lines.push(format!("[windows.{key}]"));
            if subject.bundle_id.is_empty() {
                lines.push("# bundle_id = \"\"   # unknown; rule matches every app".to_owned());
            } else {
                lines.push(format!("bundle_id = \"{bundle_id}\""));
            }
            lines.push(format!("title = \"{title}\""));
            if wildcard_alternative {
                lines.push("# title = \".*\"   # all windows of this app".to_owned());
            }
            lines.push(format!("# {identity}"));
            lines.push("# floating = true".to_owned());
            lines.push("# manage = true".to_owned());
            lines.push("# width = 0.5".to_owned());
        }
        SnippetDialect::Lua => {
            lines.push("windows = {".to_owned());
            lines.push(format!("  {key} = {{"));
            if subject.bundle_id.is_empty() {
                lines.push(
                    "    -- bundle_id = \"\",   -- unknown; rule matches every app".to_owned(),
                );
            } else {
                lines.push(format!("    bundle_id = \"{bundle_id}\","));
            }
            lines.push(format!("    title = \"{title}\","));
            if wildcard_alternative {
                lines.push("    -- title = \".*\",   -- all windows of this app".to_owned());
            }
            lines.push(format!("    -- {identity}"));
            lines.push("    -- floating = true,".to_owned());
            lines.push("    -- manage = true,".to_owned());
            lines.push("    -- width = 0.5,".to_owned());
            lines.push("  },".to_owned());
            lines.push("}".to_owned());
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Turns an application name into a table key: lowercased, with every run of
/// non-alphanumerics collapsed into a single underscore. A leading digit is
/// expanded into an English word so the key stays a valid bare identifier in
/// both TOML and Lua.
fn rule_key(app_name: &str) -> String {
    let mut key = String::with_capacity(app_name.len() + 4);
    let mut first_alnum = true;

    for character in app_name.chars() {
        if first_alnum {
            if character.is_whitespace() || !character.is_alphanumeric() {
                continue;
            }
            first_alnum = false;
            match character {
                '0' => key.push_str("zero"),
                '1' => key.push_str("one"),
                '2' => key.push_str("two"),
                '3' => key.push_str("three"),
                '4' => key.push_str("four"),
                '5' => key.push_str("five"),
                '6' => key.push_str("six"),
                '7' => key.push_str("seven"),
                '8' => key.push_str("eight"),
                '9' => key.push_str("nine"),
                c if c.is_ascii_alphanumeric() => key.push(c.to_ascii_lowercase()),
                _ => {}
            }
            continue;
        }

        if character.is_ascii_alphanumeric() {
            key.push(character.to_ascii_lowercase());
        } else if !key.ends_with('_') {
            key.push('_');
        }
    }
    let trimmed = key.trim_matches('_');
    if trimmed.is_empty() {
        "window".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The regex a rule should carry to match exactly this title. Escaped so the
/// title's own punctuation stays literal, then anchored so it does not also
/// match longer titles — `is_match` is unanchored.
fn title_pattern(title: &str) -> String {
    if title.is_empty() {
        return ".*".to_owned();
    }
    quote(&format!("^{}$", regex::escape(title)))
}

/// Escapes a string for a TOML basic string or a Lua double-quoted string. Runs
/// after `regex::escape`, whose backslashes need escaping in turn.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(character),
        }
    }
    out
}

/// The comment identifying the window. None of these are matchable, so they are
/// here only to tell the pasted rule apart from the next one.
fn identity_comment(subject: &RuleSubject<'_>) -> String {
    let name = if subject.app_name.is_empty() {
        "?"
    } else {
        subject.app_name
    };
    let role = if subject.role.is_empty() {
        "?"
    } else {
        subject.role
    };
    let subrole = if subject.subrole.is_empty() {
        "?"
    } else {
        subject.subrole
    };
    format!("app: {name}  role: {role}  subrole: {subrole}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InnerConfig;

    fn subject<'a>(app_name: &'a str, bundle_id: &'a str, title: &'a str) -> RuleSubject<'a> {
        RuleSubject {
            app_name,
            bundle_id,
            title,
            role: "AXWindow",
            subrole: "AXStandardWindow",
        }
    }

    #[test]
    fn toml_snippet_has_both_matchers() {
        let snippet = window_rule_snippet(
            SnippetDialect::Toml,
            &subject("Ghostty", "com.mitchellh.ghostty", "paneru"),
        );
        assert_eq!(
            snippet,
            concat!(
                "[windows.ghostty]\n",
                "bundle_id = \"com.mitchellh.ghostty\"\n",
                "title = \"^paneru$\"\n",
                "# title = \".*\"   # all windows of this app\n",
                "# app: Ghostty  role: AXWindow  subrole: AXStandardWindow\n",
                "# floating = true\n",
                "# manage = true\n",
                "# width = 0.5\n",
            )
        );
    }

    #[test]
    fn lua_snippet_has_both_matchers() {
        let snippet = window_rule_snippet(
            SnippetDialect::Lua,
            &subject("Ghostty", "com.mitchellh.ghostty", "paneru"),
        );
        assert_eq!(
            snippet,
            concat!(
                "windows = {\n",
                "  ghostty = {\n",
                "    bundle_id = \"com.mitchellh.ghostty\",\n",
                "    title = \"^paneru$\",\n",
                "    -- title = \".*\",   -- all windows of this app\n",
                "    -- app: Ghostty  role: AXWindow  subrole: AXStandardWindow\n",
                "    -- floating = true,\n",
                "    -- manage = true,\n",
                "    -- width = 0.5,\n",
                "  },\n",
                "}\n",
            )
        );
    }

    /// The escaping order is the part that can silently go wrong: `regex::escape`
    /// adds backslashes, which then need escaping again for the config string.
    /// Parse the snippet back and match the very window it was made from.
    #[test]
    fn generated_toml_matches_the_window_it_came_from() {
        let title = r#"Find "a.b" (2) \ [x]"#;
        let bundle_id = "com.apple.Terminal";
        let snippet = window_rule_snippet(SnippetDialect::Toml, &subject("Term", bundle_id, title));

        let config = InnerConfig::new(&snippet).expect("snippet parses as TOML config");
        let rules = config
            .windows
            .as_ref()
            .expect("snippet defines a windows table");
        let rule = rules.values().next().expect("exactly one rule");
        assert!(rule.title.is_match(title));
        assert_eq!(rule.bundle_id.as_deref(), Some(bundle_id));
    }

    #[test]
    fn anchored_pattern_rejects_a_longer_title() {
        let snippet = window_rule_snippet(SnippetDialect::Toml, &subject("Term", "com.x", "build"));
        let config = InnerConfig::new(&snippet).expect("snippet parses as TOML config");
        let rules = config.windows.as_ref().expect("windows table");
        let rule = rules.values().next().expect("exactly one rule");
        assert!(rule.title.is_match("build"));
        assert!(!rule.title.is_match("rebuild all"));
    }

    #[test]
    fn empty_title_falls_back_to_wildcard() {
        let snippet = window_rule_snippet(SnippetDialect::Toml, &subject("Term", "com.x", ""));
        assert!(snippet.contains("title = \".*\"\n"));
        assert!(!snippet.contains("# title = \".*\""));
    }

    #[test]
    fn empty_bundle_id_is_commented_out() {
        let snippet = window_rule_snippet(SnippetDialect::Toml, &subject("Term", "", "hello"));
        assert!(snippet.contains("# bundle_id = \"\"   # unknown; rule matches every app\n"));
        let config = InnerConfig::new(&snippet).expect("snippet parses as TOML config");
        let rules = config.windows.as_ref().expect("windows table");
        assert!(rules.values().next().expect("one rule").bundle_id.is_none());
    }

    #[test]
    fn rule_keys_are_sanitized() {
        assert_eq!(rule_key("Ghostty"), "ghostty");
        assert_eq!(rule_key("Visual Studio Code"), "visual_studio_code");
        assert_eq!(rule_key("1Password 8"), "onepassword_8");
        assert_eq!(rule_key("7-Zip"), "seven_zip");
        assert_eq!(rule_key("  —  "), "window");
        assert_eq!(rule_key(""), "window");
    }
}
