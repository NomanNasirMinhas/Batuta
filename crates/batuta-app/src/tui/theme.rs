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
//! ## Backgrounds come with an obligation
//!
//! The interface paints its own background: a base tone for the screen and a
//! lighter one for each panel, so the panes read as surfaces rather than as
//! text floating on whatever is behind them.
//!
//! Doing that removes the option of inheriting the terminal's foreground. Text
//! that inherits is only legible against the background it was chosen for, and
//! ours is now a known dark tone, so a light scheme's black-on-white text would
//! land black-on-charcoal. Painting a background therefore *requires* painting
//! a foreground, and both are set together, never one alone.
//!
//! This only applies where the terminal can render 24-bit colour. In sixteen
//! colours there is no tone subtle enough to sit behind a frame without
//! swallowing it, so those terminals keep the inherit-everything behaviour,
//! which is correct on any scheme. `BATUTA_NO_PANELS` opts out for anyone who
//! prefers their own background showing through.

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

/// What the surrounding environment says the terminal can do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Env<'a> {
    pub no_color: bool,
    pub colorterm: Option<&'a str>,
    pub term: Option<&'a str>,
    /// Running under Windows Terminal, which is known to draw rounded borders.
    pub windows_terminal: bool,
    /// Running on Windows at all, where 24-bit colour is available but none
    /// of the Unix capability variables are ever set.
    pub windows: bool,
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
    pub fn resolve(env: Env) -> Self {
        // NO_COLOR is a convention worth honouring exactly: any value at all,
        // including empty, means no colour.
        if env.no_color {
            return Theme {
                palette: Palette::Mono,
                rounded: env.windows_terminal,
                panels: false,
            };
        }

        // `COLORTERM` is a Unix convention. Nothing on Windows sets it, nor
        // `TERM` — so asking for it there is asking a question that is always
        // answered "no", and the interface fell back to sixteen colours on the
        // one platform this program runs on. The console has supported 24-bit
        // colour since Windows 10 1703, and crossterm turns on the virtual
        // terminal processing that uses it, so Windows is taken as capable
        // unless it says otherwise.
        let truecolor = env.windows
            || matches!(env.colorterm, Some(c) if {
                let c = c.to_ascii_lowercase();
                c.contains("truecolor") || c.contains("24bit")
            })
            || matches!(env.term, Some(t) if t.contains("256color"));

        Theme {
            palette: if truecolor {
                Palette::True
            } else {
                Palette::Ansi
            },
            // The legacy console draws rounded corners as replacement glyphs,
            // so they are only used where something is known to support them.
            // Colour degrades invisibly; a wrong glyph is a box on the screen,
            // so this stays the conservative of the two checks.
            rounded: env.windows_terminal,
            // Only truecolor can pick tones subtle enough to layer; sixteen
            // colours would swallow the frame, so those keep the terminal's
            // own background.
            panels: truecolor,
        }
    }

    /// Read the environment and decide.
    ///
    /// `BATUTA_NO_PANELS` turns off the painted background, for anyone who
    /// would rather see their own terminal's through the interface.
    pub fn detect() -> Self {
        Theme::resolve(Env {
            no_color: std::env::var_os("NO_COLOR").is_some(),
            colorterm: std::env::var("COLORTERM").ok().as_deref(),
            term: std::env::var("TERM").ok().as_deref(),
            windows_terminal: std::env::var_os("WT_SESSION").is_some(),
            windows: cfg!(windows),
        })
        .with_panels(std::env::var_os("BATUTA_NO_PANELS").is_none())
    }

    /// Turn the painted background on or off.
    pub fn with_panels(mut self, on: bool) -> Self {
        // Sixteen colours cannot do this well, so the request is refused
        // rather than honoured badly.
        self.panels = on && self.palette == Palette::True;
        self
    }

    /// The screen behind everything, or `None` to leave the terminal's own.
    pub fn bg(&self) -> Option<Color> {
        self.panels.then_some(Color::Rgb(17, 19, 24))
    }

    /// A panel's fill: one step up from [`Theme::bg`], so panes read as
    /// surfaces sitting on the screen rather than holes cut into it.
    pub fn surface(&self) -> Option<Color> {
        self.panels.then_some(Color::Rgb(26, 29, 36))
    }

    /// Body text.
    ///
    /// `Reset` inherits the terminal's foreground, which is right whenever we
    /// have not painted a background. Once we have, inheriting would put a
    /// light scheme's black text on our charcoal, so an explicit tone is used
    /// instead. The two decisions are the same decision.
    pub fn text(&self) -> Color {
        if self.panels {
            Color::Rgb(222, 226, 233)
        } else {
            Color::Reset
        }
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
            (Palette::True, Mode::Explore) => Color::Rgb(74, 222, 128),
            (Palette::Ansi, Mode::Search) => Color::Cyan,
            (Palette::Ansi, Mode::Bloat) => Color::LightMagenta,
            (Palette::Ansi, Mode::Dupes) => Color::Yellow,
            (Palette::Ansi, Mode::Explore) => Color::Green,
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
        let t = Theme::resolve(Env {
            no_color: true,
            colorterm: Some("truecolor"),
            term: Some("xterm-256color"),
            windows_terminal: true,
            windows: true,
        });
        assert_eq!(t.palette, Palette::Mono);
        assert_eq!(t.bg(), None, "no colour means no painted background");
        assert_eq!(t.text(), Color::Reset);
        assert_eq!(t.heat(100 * GIB), None, "no colour means no heat either");
        assert_eq!(t.accent(Mode::Search), Color::Reset);
    }

    #[test]
    fn windows_gets_full_colour_without_any_unix_variables_being_set() {
        // The regression this pins: `COLORTERM`, `TERM` and `WT_SESSION` are
        // all unset on a stock Windows console, so asking only those questions
        // dropped the one platform this program runs on to sixteen colours and
        // no painted background at all.
        let t = Theme::resolve(Env {
            windows: true,
            ..Env::default()
        });
        assert_eq!(t.palette, Palette::True);
        assert!(t.bg().is_some(), "the background must actually be painted");

        // NO_COLOR still wins there, as everywhere.
        let off = Theme::resolve(Env {
            windows: true,
            no_color: true,
            ..Env::default()
        });
        assert_eq!(off.palette, Palette::Mono);
        assert_eq!(off.bg(), None);
    }

    #[test]
    fn truecolor_is_taken_from_colorterm() {
        for c in ["truecolor", "24bit", "TrueColor"] {
            let t = Theme::resolve(Env {
                colorterm: Some(c),
                ..Env::default()
            });
            assert_eq!(t.palette, Palette::True, "COLORTERM={c}");
        }
    }

    #[test]
    fn an_unknown_terminal_falls_back_to_ansi_rather_than_guessing() {
        let t = Theme::resolve(Env {
            term: Some("dumb"),
            ..Env::default()
        });
        assert_eq!(t.palette, Palette::Ansi);
        // Every token must still resolve to something drawable.
        assert_eq!(t.border_type(), BorderType::Plain);
        assert!(t.heat(2 * GIB).is_some());
    }

    #[test]
    fn rounded_borders_are_only_used_where_they_render() {
        // The legacy Windows console draws them as replacement glyphs.
        assert_eq!(
            Theme::resolve(Env {
                colorterm: Some("truecolor"),
                ..Env::default()
            })
            .border_type(),
            BorderType::Plain
        );
        assert_eq!(
            Theme::resolve(Env {
                colorterm: Some("truecolor"),
                windows_terminal: true,
                ..Env::default()
            })
            .border_type(),
            BorderType::Rounded
        );
    }

    #[test]
    fn a_painted_background_always_brings_a_foreground_with_it() {
        // The pairing is the whole safety property: a background without an
        // explicit foreground leaves a light scheme's black text on charcoal.
        let painted = Theme::resolve(Env {
            colorterm: Some("truecolor"),
            windows_terminal: true,
            ..Env::default()
        });
        assert!(painted.bg().is_some(), "truecolor should paint by default");
        assert!(painted.surface().is_some());
        assert_ne!(
            painted.text(),
            Color::Reset,
            "painting a background obliges us to name the text colour"
        );

        // And the converse: inherit the foreground exactly when we inherit
        // the background.
        let bare = painted.with_panels(false);
        assert_eq!(bare.bg(), None);
        assert_eq!(bare.text(), Color::Reset);
    }

    #[test]
    fn sixteen_colour_terminals_keep_their_own_background() {
        // No tone here is subtle enough to sit behind a frame.
        let ansi = Theme::resolve(Env {
            windows_terminal: true,
            ..Env::default()
        });
        assert_eq!(ansi.bg(), None);
        assert_eq!(ansi.text(), Color::Reset);
        assert_eq!(ansi.with_panels(true).bg(), None, "request refused");
    }

    #[test]
    fn a_panel_is_a_step_up_from_the_screen_behind_it() {
        let t = Theme::resolve(Env {
            colorterm: Some("truecolor"),
            windows_terminal: true,
            ..Env::default()
        });
        assert_ne!(t.bg(), t.surface(), "panels must be distinguishable");
    }

    #[test]
    fn heat_only_marks_sizes_worth_acting_on() {
        let t = Theme::resolve(Env {
            colorterm: Some("truecolor"),
            windows_terminal: true,
            ..Env::default()
        });
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
