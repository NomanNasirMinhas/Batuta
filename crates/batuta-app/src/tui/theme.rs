//! Colour and glyph tokens for the TUI.
//!
//! Every colour in the interface comes from here rather than being named at
//! the point of use. Two reasons, both learned the hard way from the previous
//! pass:
//!
//! 1. **Bare ANSI names are at the mercy of the user's scheme.** `Color::Cyan`
//!    is whatever the terminal decides it is, which on some light schemes is
//!    close to the background and effectively invisible. Tokens let the whole
//!    palette move together when the terminal cannot do better.
//! 2. **Not every terminal can draw the same frame.** Rounded borders render as
//!    replacement boxes in the legacy Windows console, so the border style is a
//!    token too, not a literal at forty call sites.
//!
//! ## Why nothing has a filled background by default
//!
//! Panels look layered in a screenshot because the author knew their terminal's
//! background. We do not: a terminal reports no way to ask. Painting a dark
//! panel behind text that inherits a light foreground produces black-on-black,
//! so depth comes from borders and dimmed text instead, which is correct
//! against any background. Someone who *knows* their terminal is dark can opt
//! into filled panels by setting `BATUTA_PANELS`, and then the risk is theirs
//! to take knowingly. Body text simply never sets a foreground at all, which
//! is what makes it correct on a light scheme and a dark one alike.

use ratatui::style::Color;
use ratatui::widgets::BorderType;

use super::app::Mode;

/// How much colour the terminal can actually render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    /// 24-bit colour: the full token set.
    True,
    /// The 16 ANSI colours, whatever the user's scheme maps them to.
    Ansi,
    /// No colour at all. Emphasis has to come from bold and reverse video.
    Mono,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub palette: Palette,
    /// Rounded corners need a terminal that can draw them.
    pub rounded: bool,
    /// Fill panel backgrounds. Off unless asked for; see the module note.
    pub panels: bool,
}

/// Sizes worth acting on, in bytes.
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

impl Default for Theme {
    fn default() -> Self {
        Theme {
            palette: Palette::Ansi,
            rounded: false,
            panels: false,
        }
    }
}

impl Theme {
    /// Work out the theme from the environment.
    ///
    /// Split from [`Theme::detect`] so the rules are testable without setting
    /// process-wide environment variables, which tests cannot do safely in
    /// parallel.
    pub fn resolve(
        no_color: bool,
        colorterm: Option<&str>,
        term: Option<&str>,
        windows_terminal: bool,
    ) -> Self {
        // NO_COLOR is a convention worth honouring exactly: any value at all,
        // including empty, means no colour.
        if no_color {
            return Theme {
                palette: Palette::Mono,
                rounded: windows_terminal,
                panels: false,
            };
        }

        let truecolor = matches!(colorterm, Some(c) if {
            let c = c.to_ascii_lowercase();
            c.contains("truecolor") || c.contains("24bit")
        }) || matches!(term, Some(t) if t.contains("256color") && windows_terminal);

        Theme {
            palette: if truecolor {
                Palette::True
            } else {
                Palette::Ansi
            },
            // The legacy console draws rounded corners as replacement glyphs,
            // so they are only used where something is known to support them.
            rounded: windows_terminal,
            panels: false,
        }
    }

    /// Read the environment and decide.
    ///
    /// `BATUTA_PANELS` opts into filled panel backgrounds. It is a deliberate
    /// switch rather than a default because only the person at the keyboard
    /// knows whether their terminal's own background is dark enough for it.
    pub fn detect() -> Self {
        Theme::resolve(
            std::env::var_os("NO_COLOR").is_some(),
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
            std::env::var_os("WT_SESSION").is_some(),
        )
        .with_panels(std::env::var_os("BATUTA_PANELS").is_some())
    }

    /// Turn on filled panel backgrounds, for a terminal known to be dark.
    pub fn with_panels(mut self, on: bool) -> Self {
        // Only truecolor can pick a background subtle enough to be worth it;
        // an ANSI black would swallow the frame.
        self.panels = on && self.palette == Palette::True;
        self
    }

    pub fn border_type(&self) -> BorderType {
        if self.rounded {
            BorderType::Rounded
        } else {
            BorderType::Plain
        }
    }

    /// Secondary text: present but not competing.
    pub fn dim(&self) -> Color {
        match self.palette {
            // Mid-grey on purpose: it stays readable against black and white
            // alike, where a darker grey would vanish on one of them.
            Palette::True => Color::Rgb(122, 132, 147),
            Palette::Ansi | Palette::Mono => Color::DarkGray,
        }
    }

    /// An unfocused frame.
    pub fn border(&self) -> Color {
        match self.palette {
            Palette::True => Color::Rgb(68, 76, 90),
            Palette::Ansi | Palette::Mono => Color::DarkGray,
        }
    }

    /// The mode's accent: the one colour that ties the frame, the active rail
    /// entry and the selection bar together.
    pub fn accent(&self, mode: Mode) -> Color {
        match (self.palette, mode) {
            (Palette::Mono, _) => Color::Reset,
            (Palette::True, Mode::Search) => Color::Rgb(56, 189, 248),
            (Palette::True, Mode::Bloat) => Color::Rgb(192, 132, 252),
            (Palette::True, Mode::Dupes) => Color::Rgb(250, 204, 21),
            (Palette::Ansi, Mode::Search) => Color::Cyan,
            (Palette::Ansi, Mode::Bloat) => Color::LightMagenta,
            (Palette::Ansi, Mode::Dupes) => Color::Yellow,
        }
    }

    /// Text drawn on top of an accent-filled bar.
    pub fn on_accent(&self) -> Color {
        match self.palette {
            // Reversed video supplies the contrast when there is no colour.
            Palette::Mono => Color::Reset,
            _ => Color::Black,
        }
    }

    pub fn danger(&self) -> Color {
        match self.palette {
            Palette::True => Color::Rgb(248, 113, 113),
            Palette::Ansi => Color::LightRed,
            Palette::Mono => Color::Reset,
        }
    }

    pub fn warn(&self) -> Color {
        match self.palette {
            Palette::True => Color::Rgb(250, 204, 21),
            Palette::Ansi => Color::Yellow,
            Palette::Mono => Color::Reset,
        }
    }

    pub fn ok(&self) -> Color {
        match self.palette {
            Palette::True => Color::Rgb(74, 222, 128),
            Palette::Ansi => Color::Green,
            Palette::Mono => Color::Reset,
        }
    }

    /// A filled panel background, or `None` to inherit the terminal's.
    pub fn panel(&self) -> Option<Color> {
        self.panels.then_some(Color::Rgb(24, 27, 33))
    }

    /// How loudly a size should read.
    ///
    /// The question a size column answers is "is this one worth acting on",
    /// so the scale is deliberately coarse: below 100 MiB nothing is tinted at
    /// all, because tinting everything is the same as tinting nothing.
    pub fn heat(&self, size: u64) -> Option<Color> {
        match self.palette {
            Palette::Mono => None,
            Palette::True => {
                if size >= GIB {
                    Some(Color::Rgb(248, 113, 113))
                } else if size >= 400 * MIB {
                    Some(Color::Rgb(251, 146, 60))
                } else if size >= 100 * MIB {
                    Some(Color::Rgb(250, 204, 21))
                } else {
                    None
                }
            }
            Palette::Ansi => {
                if size >= GIB {
                    Some(Color::LightRed)
                } else if size >= 100 * MIB {
                    Some(Color::LightYellow)
                } else {
                    None
                }
            }
        }
    }
}

/// The process-wide theme, resolved once.
///
/// Rendering runs on one thread and the theme never changes after startup, so
/// threading it through every function would be churn for nothing. The rules
/// that decide it are pure and tested directly on [`Theme::resolve`].
pub fn theme() -> &'static Theme {
    use std::sync::OnceLock;
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(Theme::detect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_wins_over_everything_else() {
        // The convention is that any value, empty included, disables colour.
        let t = Theme::resolve(true, Some("truecolor"), Some("xterm-256color"), true);
        assert_eq!(t.palette, Palette::Mono);
        assert_eq!(t.heat(100 * GIB), None, "no colour means no heat either");
        assert_eq!(t.accent(Mode::Search), Color::Reset);
    }

    #[test]
    fn truecolor_is_taken_from_colorterm() {
        for c in ["truecolor", "24bit", "TrueColor"] {
            let t = Theme::resolve(false, Some(c), None, false);
            assert_eq!(t.palette, Palette::True, "COLORTERM={c}");
        }
    }

    #[test]
    fn an_unknown_terminal_falls_back_to_ansi_rather_than_guessing() {
        let t = Theme::resolve(false, None, Some("dumb"), false);
        assert_eq!(t.palette, Palette::Ansi);
        // Every token must still resolve to something drawable.
        assert_eq!(t.border_type(), BorderType::Plain);
        assert!(t.heat(2 * GIB).is_some());
    }

    #[test]
    fn rounded_borders_are_only_used_where_they_render() {
        // The legacy Windows console draws them as replacement glyphs.
        assert_eq!(
            Theme::resolve(false, Some("truecolor"), None, false).border_type(),
            BorderType::Plain
        );
        assert_eq!(
            Theme::resolve(false, Some("truecolor"), None, true).border_type(),
            BorderType::Rounded
        );
    }

    #[test]
    fn panels_stay_off_unless_asked_for_and_affordable() {
        let truecolor = Theme::resolve(false, Some("truecolor"), None, true);
        assert_eq!(truecolor.panel(), None, "off by default");
        assert!(truecolor.with_panels(true).panel().is_some());

        // A filled panel in 16 colours would swallow the frame, so the request
        // is refused rather than honoured badly.
        let ansi = Theme::resolve(false, None, None, true);
        assert_eq!(ansi.with_panels(true).panel(), None);
    }

    #[test]
    fn heat_only_marks_sizes_worth_acting_on() {
        let t = Theme::resolve(false, Some("truecolor"), None, true);
        assert_eq!(t.heat(0), None);
        assert_eq!(t.heat(99 * MIB), None, "small files must stay unmarked");
        assert!(t.heat(101 * MIB).is_some());
        assert_ne!(
            t.heat(2 * GIB),
            t.heat(101 * MIB),
            "a gigabyte must not read the same as a hundred megabytes"
        );
    }

    #[test]
    fn every_mode_has_its_own_accent() {
        for p in [Palette::True, Palette::Ansi] {
            let t = Theme {
                palette: p,
                rounded: false,
                panels: false,
            };
            let (s, b, d) = (
                t.accent(Mode::Search),
                t.accent(Mode::Bloat),
                t.accent(Mode::Dupes),
            );
            assert_ne!(s, b, "{p:?}");
            assert_ne!(b, d, "{p:?}");
            assert_ne!(s, d, "{p:?}");
        }
    }
}
