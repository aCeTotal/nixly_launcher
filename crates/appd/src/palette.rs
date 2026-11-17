use std::path::{Path, PathBuf};

use crate::render::Color;

pub const FILE: &str = "nixlytile.conf";

const THEME_DIR: &str = ".local/state/nixly-theme";
const ROLE_PREFIX: &str = "dark.";
const HEX_DIGITS: usize = 6;
const HEX_RADIX: u32 = 16;

const OPAQUE: f32 = 1.0;
const PANEL_ALPHA: f32 = 0.94;
const TRACK_ALPHA: f32 = 0.6;

const BG: Color = [0.06, 0.06, 0.08, PANEL_ALPHA];
const FG: Color = [0.92, 0.92, 0.94, OPAQUE];
const FG_DIM: Color = [0.55, 0.55, 0.58, OPAQUE];
const ACCENT: Color = [0.27, 0.55, 0.95, OPAQUE];
const SEL_BG: Color = [0.20, 0.22, 0.30, OPAQUE];
const TRACK: Color = [0.18, 0.20, 0.26, TRACK_ALPHA];
const SLATE: Color = [0.478, 0.533, 0.659, OPAQUE];

#[derive(Debug, PartialEq)]
pub struct Palette {
    pub panel: Color,
    pub text: Color,
    pub text_dim: Color,
    pub placeholder: Color,
    pub accent: Color,
    pub selection: Color,
    pub on_selection: Color,
    pub divider: Color,
    pub track: Color,
    pub glyph: Color,
}

impl Palette {
    pub const FALLBACK: Palette = Palette {
        panel: BG,
        text: FG,
        text_dim: FG_DIM,
        placeholder: FG_DIM,
        accent: ACCENT,
        selection: SEL_BG,
        on_selection: FG,
        divider: SEL_BG,
        track: TRACK,
        glyph: SLATE,
    };
}

enum Role {
    Container,
    OnSurface,
    OnVariant,
    Outline,
    OutlineVariant,
    Primary,
    SecondaryContainer,
    OnSecondaryContainer,
}

// Indexed by `Role as usize`.
const ROLE_NAMES: [&str; 8] = [
    "surface_container",
    "on_surface",
    "on_surface_variant",
    "outline",
    "outline_variant",
    "primary",
    "secondary_container",
    "on_secondary_container",
];

type Rgb = [f32; 3];

pub fn dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(THEME_DIR))
}

pub fn current() -> Option<Palette> {
    load(&dir()?.join(FILE))
}

fn load(path: &Path) -> Option<Palette> {
    parse(&std::fs::read_to_string(path).ok()?)
}

// None unless all roles present.
fn parse(text: &str) -> Option<Palette> {
    let mut found: [Option<Rgb>; ROLE_NAMES.len()] = [None; ROLE_NAMES.len()];
    for (name, rgb) in text.lines().filter_map(role_line) {
        if let Some(i) = ROLE_NAMES.iter().position(|r| *r == name) {
            found[i] = Some(rgb);
        }
    }
    let mut roles = [[0.0; 3]; ROLE_NAMES.len()];
    for (slot, rgb) in roles.iter_mut().zip(found) {
        *slot = rgb?;
    }
    Some(from_roles(&roles))
}

fn from_roles(roles: &[Rgb; ROLE_NAMES.len()]) -> Palette {
    let role = |r: Role, alpha: f32| {
        let [red, green, blue] = roles[r as usize];
        [red, green, blue, alpha]
    };
    Palette {
        panel: role(Role::Container, PANEL_ALPHA),
        text: role(Role::OnSurface, OPAQUE),
        text_dim: role(Role::OnVariant, OPAQUE),
        placeholder: role(Role::Outline, OPAQUE),
        accent: role(Role::Primary, OPAQUE),
        selection: role(Role::SecondaryContainer, OPAQUE),
        on_selection: role(Role::OnSecondaryContainer, OPAQUE),
        divider: role(Role::OutlineVariant, OPAQUE),
        track: role(Role::OutlineVariant, TRACK_ALPHA),
        glyph: role(Role::OnVariant, OPAQUE),
    }
}

// "dark.<role> rrggbb"
fn role_line(line: &str) -> Option<(&str, Rgb)> {
    let (name, hex) = line.strip_prefix(ROLE_PREFIX)?.split_once(' ')?;
    let hex = hex.trim();
    if hex.len() != HEX_DIGITS {
        return None;
    }
    let [_, r, g, b] = u32::from_str_radix(hex, HEX_RADIX).ok()?.to_be_bytes();
    Some((name, [r, g, b].map(|c| f32::from(c) / f32::from(u8::MAX))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETE: &str = "image /tmp/wall.png
dark.background 19120c
dark.on_secondary_container feddbd
dark.on_surface eee0d5
dark.on_surface_variant d4c4b5
dark.outline 9d8e81
dark.outline_variant 50453a
dark.primary faba73
dark.secondary_container 58432c
dark.surface_container 251e17
";

    fn rgb(hex: u32) -> Rgb {
        let [_, r, g, b] = hex.to_be_bytes();
        [r, g, b].map(|c| f32::from(c) / f32::from(u8::MAX))
    }

    #[test]
    fn complete_file_maps_roles() {
        let p = parse(COMPLETE).expect("complete palette");
        let [r, g, b] = rgb(0x251e17);
        assert_eq!(p.panel, [r, g, b, PANEL_ALPHA]);
        assert_eq!(p.text[..3], rgb(0xeee0d5));
        assert_eq!(p.placeholder[..3], rgb(0x9d8e81));
        assert_eq!(p.accent[..3], rgb(0xfaba73));
        assert_eq!(p.selection[..3], rgb(0x58432c));
        assert_eq!(p.on_selection[..3], rgb(0xfeddbd));
        assert_eq!(p.track[3], TRACK_ALPHA);
    }

    #[test]
    fn partial_file_is_rejected() {
        let truncated = &COMPLETE[..COMPLETE.find("dark.primary").unwrap()];
        assert_eq!(parse(truncated), None);
    }

    #[test]
    fn half_written_value_is_rejected() {
        let cut = COMPLETE.trim_end().trim_end_matches("251e17").to_owned() + "25";
        assert_eq!(parse(&cut), None);
    }

    #[test]
    fn truncated_to_empty_is_rejected() {
        assert_eq!(parse(""), None);
    }

    #[test]
    fn missing_file_is_rejected() {
        assert_eq!(load(Path::new("/nonexistent/nixlytile.conf")), None);
    }

    #[test]
    fn crlf_and_unknown_roles_are_tolerated() {
        let crlf = COMPLETE.replace('\n', "\r\n") + "dark.bogus zzzzzz\r\n";
        assert_eq!(parse(&crlf), parse(COMPLETE));
    }
}
