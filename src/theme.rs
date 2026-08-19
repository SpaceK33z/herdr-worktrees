//! Theme colors for plugin-owned terminal UI.
//!
//! Herdr does not currently expose its resolved palette to plugins, so this
//! module mirrors the built-in accent/teal tokens and applies matching
//! `[theme.custom]` overrides from Herdr's config. Unknown themes fall back to
//! portable ANSI colors.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeColors {
    pub worktrees: AnsiColor,
    pub branches: AnsiColor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiColor(String);

impl AnsiColor {
    fn rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(format!("38;2;{red};{green};{blue}"))
    }

    fn indexed(code: u8) -> Self {
        Self(code.to_string())
    }

    pub fn paint(&self, text: &str) -> String {
        let reapply = format!("\x1b[0m\x1b[{}m", self.0);
        format!(
            "\x1b[{}m{}\x1b[0m",
            self.0,
            text.replace("\x1b[0m", &reapply)
        )
    }

    pub fn paint_bold(&self, text: &str) -> String {
        format!("\x1b[1;{}m{text}\x1b[0m", self.0)
    }
}

impl Default for ThemeColors {
    fn default() -> Self {
        palette("catppuccin")
    }
}

impl ThemeColors {
    pub fn load() -> Self {
        let Some(config) = read_herdr_config() else {
            return Self::default();
        };
        resolve(&config)
    }
}

fn read_herdr_config() -> Option<toml::Value> {
    let path = std::env::var("HERDR_CONFIG_PATH")
        .ok()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|path| !path.is_empty())
                .map(|path| PathBuf::from(path).join("herdr/config.toml"))
        })
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|home| PathBuf::from(home).join(".config/herdr/config.toml"))
        })?;
    let source = std::fs::read_to_string(path).ok()?;
    toml::from_str(&source).ok()
}

fn resolve(config: &toml::Value) -> ThemeColors {
    let theme = config.get("theme").and_then(toml::Value::as_table);
    let name = theme.and_then(active_theme_name).unwrap_or("catppuccin");
    let mut colors = palette(name);

    if let Some(custom) = theme
        .and_then(|theme| theme.get("custom"))
        .and_then(toml::Value::as_table)
    {
        if let Some(color) = custom
            .get("accent")
            .and_then(toml::Value::as_str)
            .and_then(parse_color)
        {
            colors.worktrees = color;
        }
        if let Some(color) = custom
            .get("teal")
            .and_then(toml::Value::as_str)
            .and_then(parse_color)
        {
            colors.branches = color;
        }
    }
    colors
}

fn active_theme_name(theme: &toml::map::Map<String, toml::Value>) -> Option<&str> {
    let auto_switch = theme
        .get("auto_switch")
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    if auto_switch {
        let key = if terminal_looks_light() {
            "light_name"
        } else {
            "dark_name"
        };
        if let Some(name) = theme.get(key).and_then(toml::Value::as_str) {
            return Some(name);
        }
    }
    theme.get("name").and_then(toml::Value::as_str)
}

fn terminal_looks_light() -> bool {
    std::env::var("COLORFGBG")
        .ok()
        .and_then(|value| value.rsplit(';').next()?.parse::<u8>().ok())
        .is_some_and(|background| background >= 7)
}

fn palette(name: &str) -> ThemeColors {
    let normalized = name.to_lowercase().replace([' ', '_'], "-");
    let (accent, teal) = match normalized.as_str() {
        "catppuccin" | "catppuccin-mocha" => ((137, 180, 250), (148, 226, 213)),
        "catppuccin-latte" | "latte" | "light" => ((30, 102, 245), (23, 146, 153)),
        "tokyo-night" | "tokyonight" => ((122, 162, 247), (125, 207, 255)),
        "tokyo-night-day" | "tokyo-day" | "tokyonight-day" => ((46, 125, 233), (17, 140, 116)),
        "dracula" => ((189, 147, 249), (139, 233, 253)),
        "nord" => ((136, 192, 208), (143, 188, 187)),
        "gruvbox" | "gruvbox-dark" => ((215, 153, 33), (142, 192, 124)),
        "gruvbox-light" => ((7, 102, 120), (66, 123, 88)),
        "one-dark" | "onedark" => ((97, 175, 239), (86, 182, 194)),
        "one-light" | "onelight" => ((64, 120, 242), (1, 132, 188)),
        "solarized" | "solarized-dark" => ((38, 139, 210), (42, 161, 152)),
        "solarized-light" => ((38, 139, 210), (42, 161, 152)),
        "kanagawa" => ((126, 156, 216), (127, 180, 202)),
        "kanagawa-lotus" | "lotus" => ((77, 105, 155), (78, 140, 162)),
        "rose-pine" | "rosepine" => ((196, 167, 231), (156, 207, 216)),
        "rose-pine-dawn" | "rosepine-dawn" | "dawn" => ((144, 122, 169), (86, 148, 159)),
        "vesper" => ((255, 199, 153), (102, 221, 204)),
        "terminal" => {
            return ThemeColors {
                worktrees: AnsiColor::indexed(34),
                branches: AnsiColor::indexed(36),
            };
        }
        _ => {
            return ThemeColors {
                worktrees: AnsiColor::indexed(36),
                branches: AnsiColor::indexed(35),
            };
        }
    };
    ThemeColors {
        worktrees: AnsiColor::rgb(accent.0, accent.1, accent.2),
        branches: AnsiColor::rgb(teal.0, teal.1, teal.2),
    }
}

fn parse_color(value: &str) -> Option<AnsiColor> {
    let value = value.trim().to_lowercase();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 {
            return Some(AnsiColor::rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ));
        }
    }
    if let Some(values) = value.strip_prefix("rgb(").and_then(|v| v.strip_suffix(')')) {
        let channels: Vec<u8> = values
            .split(',')
            .map(str::trim)
            .map(str::parse)
            .collect::<Result<_, _>>()
            .ok()?;
        if channels.len() == 3 {
            return Some(AnsiColor::rgb(channels[0], channels[1], channels[2]));
        }
    }
    let code = match value.as_str() {
        "black" => 30,
        "red" => 31,
        "green" => 32,
        "yellow" => 33,
        "blue" => 34,
        "magenta" | "purple" | "mauve" => 35,
        "cyan" | "teal" => 36,
        "white" | "gray" | "grey" => 37,
        "dark-gray" | "darkgray" | "dark-grey" | "darkgrey" => 90,
        "light-red" | "lightred" => 91,
        "light-green" | "lightgreen" => 92,
        "light-yellow" | "lightyellow" => 93,
        "light-blue" | "lightblue" => 94,
        "light-magenta" | "lightmagenta" => 95,
        "light-cyan" | "lightcyan" => 96,
        _ => return None,
    };
    Some(AnsiColor::indexed(code))
}

#[cfg(test)]
mod tests {
    use super::{resolve, AnsiColor};

    #[test]
    fn uses_builtin_palette_and_custom_overrides() {
        let config: toml::Value = toml::from_str(
            r##"
            [theme]
            name = "gruvbox"

            [theme.custom]
            accent = "#010203"
            teal = "rgb(4, 5, 6)"
            "##,
        )
        .unwrap();
        let colors = resolve(&config);
        assert_eq!(colors.worktrees, AnsiColor::rgb(1, 2, 3));
        assert_eq!(colors.branches, AnsiColor::rgb(4, 5, 6));
    }
}
