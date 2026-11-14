//! Bounded, pure decoding of the color fields used by Omarchy palettes.
//!
//! This deliberately reads only top-level RGB string assignments. Unknown
//! metadata is inert; templates, shell fragments and escape sequences are
//! never interpreted. Role mapping matches `themes/omarchy/generate.py`.

use super::{Theme, MAX_OMARCHY_PALETTE_BYTES};
use crate::Color;

const KEYS: [&str; 14] = [
    "background",
    "foreground",
    "lighter_background",
    "accent",
    "blue",
    "color4",
    "muted",
    "color8",
    "green",
    "color2",
    "yellow",
    "color3",
    "red",
    "color1",
];

impl Theme {
    /// Decode a bounded active Omarchy palette without I/O or executing its contents.
    /// Missing required roles or malformed selected values keep callers on their
    /// last valid palette rather than silently substituting a different theme.
    pub fn from_omarchy_colors(text: &str) -> Result<Self, String> {
        if text.len() > MAX_OMARCHY_PALETTE_BYTES {
            return Err("Omarchy palette exceeds 64 KiB".into());
        }
        let mut values = [None; KEYS.len()];
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // A table cannot supply the required top-level roles. Stop rather
            // than accidentally accepting same-named keys from a nested table.
            if line.starts_with('[') {
                break;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err("Omarchy palette contains an incomplete assignment".into());
            };
            let key = key.trim();
            let Some(index) = KEYS.iter().position(|known| *known == key) else {
                continue;
            };
            if values[index].is_some() {
                return Err(format!("Omarchy palette repeats {key}"));
            }
            values[index] = Some(value.trim());
        }
        let role = |indices: &[usize]| -> Result<Option<Color>, String> {
            for &index in indices {
                if let Some(value) = values[index] {
                    return rgb_string(value).map(Some).ok_or_else(|| {
                        format!(
                            "Omarchy {} must be a quoted six-digit RGB color",
                            KEYS[index]
                        )
                    });
                }
            }
            Ok(None)
        };
        let required = |indices: &[usize]| -> Result<Color, String> {
            role(indices)?.ok_or_else(|| format!("Omarchy palette is missing {}", KEYS[indices[0]]))
        };
        let background = required(&[0])?;
        let foreground = required(&[1])?;
        let blended = blend(background, foreground);
        let mut surface = role(&[2])?.unwrap_or(blended);
        let mut accent = required(&[3, 4, 5])?;
        let muted = role(&[6, 7])?.unwrap_or(foreground);
        let success = required(&[8, 9])?;
        let warning = required(&[10, 11])?;
        let error = required(&[12, 13])?;
        if surface == foreground {
            surface = blended;
        }
        if accent == surface {
            accent = foreground;
        }
        Ok(Self::semantic(
            background, surface, foreground, muted, accent, success, warning, error,
        ))
    }
}

fn rgb_string(value: &str) -> Option<Color> {
    let quote = *value.as_bytes().first()?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let end = value[1..].find(char::from(quote))? + 1;
    let trailing = value[end + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') {
        return None;
    }
    let value = &value[1..end];
    let hex = value
        .strip_prefix('#')
        .or_else(|| value.strip_prefix("0x"))
        .unwrap_or(value);
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let color = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::Rgb(
        (color >> 16) as u8,
        (color >> 8) as u8,
        color as u8,
    ))
}

fn blend(background: Color, foreground: Color) -> Color {
    let (Color::Rgb(br, bg, bb), Color::Rgb(fr, fg, fb)) = (background, foreground) else {
        unreachable!("RGB parser supplies only RGB colors")
    };
    let channel = |background: u8, foreground: u8| {
        ((u16::from(background) * 92 + u16::from(foreground) * 8) / 100) as u8
    };
    Color::Rgb(channel(br, fr), channel(bg, fg), channel(bb, fb))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: &str = r##"
mode = "dark"
background = "#101010"
foreground = '#f0f0f0' # trailing comment
color1 = "#ff0000"
color2 = "#00ff00"
color3 = "#ffff00"
color4 = "#0000ff"
color8 = "#808080"
"##;

    #[test]
    fn live_omarchy_parser_matches_every_bundled_colors_source() {
        let snapshot: serde_json::Value =
            serde_json::from_str(include_str!("../../../../themes/omarchy/sources.json")).unwrap();
        let mut checked = 0;
        for source in snapshot["sources"].as_array().unwrap() {
            if source.get("error").is_some()
                || !source["path"].as_str().unwrap().ends_with("colors.toml")
            {
                continue;
            }
            let prefix = match source["origin"].as_str().unwrap() {
                "bundled" => "omarchy-",
                "gallery" => "omarchy-community-",
                "local" | "local-bundled" => "omarchy-local-",
                origin => panic!("unexpected catalog origin {origin}"),
            };
            let slug = source["name"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .split(|ch: char| !ch.is_ascii_alphanumeric())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            let name = format!("{prefix}{slug}");
            let actual = Theme::from_omarchy_colors(source["content"].as_str().unwrap())
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(actual, Theme::named(&name), "{name}");
            checked += 1;
        }
        assert!(
            checked >= 100,
            "the test must cover the actual catalog, got {checked}"
        );
    }

    #[test]
    fn omarchy_roles_and_aliases_match_catalog_mapping() {
        let theme = Theme::from_omarchy_colors(PALETTE).unwrap();
        assert_eq!(theme.background, Color::Rgb(16, 16, 16));
        assert_eq!(theme.foreground, Color::Rgb(240, 240, 240));
        assert_eq!(theme.surface, Color::Rgb(33, 33, 33));
        assert_eq!(theme.muted, Color::Rgb(128, 128, 128));
        assert_eq!(theme.accent, Color::Rgb(0, 0, 255));
        assert_eq!(theme.success, Color::Rgb(0, 255, 0));
        assert_eq!(theme.warning, Color::Rgb(255, 255, 0));
        assert_eq!(theme.error, Color::Rgb(255, 0, 0));
        let aliases = format!(
            "{PALETTE}accent = \"#121212\"\nblue = \"#232323\"\nmuted = \"#343434\"\ngreen = \"0x454545\"\nyellow = \"565656\"\nred = \"#676767\"\nlighter_background = \"#787878\"\n"
        );
        let theme = Theme::from_omarchy_colors(&aliases).unwrap();
        assert_eq!(theme.surface, Color::Rgb(120, 120, 120));
        assert_eq!(theme.accent, Color::Rgb(18, 18, 18));
        assert_eq!(theme.muted, Color::Rgb(52, 52, 52));
        assert_eq!(theme.success, Color::Rgb(69, 69, 69));
        assert_eq!(theme.warning, Color::Rgb(86, 86, 86));
        assert_eq!(theme.error, Color::Rgb(103, 103, 103));
    }

    #[test]
    fn omarchy_palette_rejects_incomplete_unsafe_and_oversized_input() {
        for invalid in [
            "",
            "[palette]\n",
            "foreground = \"#ffffff\"\n",
            "background = \"#12\"\n",
        ] {
            assert!(Theme::from_omarchy_colors(invalid).is_err());
        }
        for value in [
            "\"#ffffff",
            "\"#ffffff\" junk",
            "\"#ffffff\"; evil",
            "#ffffff",
            "\"#éabc\"",
            "\"\\u001b[31m\"",
            "\"#ff0000\\\"\"",
            "\"#ffffff\"\nbackground = \"#000000\"",
        ] {
            let text = PALETTE.replace("\"#101010\"", value);
            assert!(Theme::from_omarchy_colors(&text).is_err(), "{value:?}");
        }
        assert!(Theme::from_omarchy_colors(&"#".repeat(MAX_OMARCHY_PALETTE_BYTES + 1)).is_err());
        assert!(Theme::from_omarchy_colors(&format!("{PALETTE}\naccent =")).is_err());
        assert!(Theme::from_omarchy_colors(&format!("{PALETTE}\nunfinished")).is_err());
    }

    #[test]
    fn omarchy_monochrome_fallbacks_keep_chrome_visible_and_roundtrip_on_wire() {
        let text = format!("{PALETTE}lighter_background = \"#f0f0f0\"\naccent = \"#212121\"\n");
        let theme = Theme::from_omarchy_colors(&text).unwrap();
        assert_eq!(theme.surface, Color::Rgb(33, 33, 33));
        assert_eq!(theme.accent, theme.foreground);
        let wire = serde_json::to_string(&theme).unwrap();
        assert_eq!(serde_json::from_str::<Theme>(&wire).unwrap(), theme);
    }
}
