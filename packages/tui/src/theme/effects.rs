//! Terminal adaptation of the decorative `[chrome.tui]` choices that never
//! change layout geometry: status separators, surface layering, and motion.
//!
//! * Separators pick the glyph joining status line segments. Font-dependent
//!   glyphs (Nerd Font powerline and slant symbols) fall back to Unicode and
//!   then ASCII under the same icon-set rules as header icons, and every
//!   candidate is measured by BMUX so a separator never changes width when it
//!   falls back.
//! * Surfaces choose whether tool units and group headers sit on the raised
//!   surface. Layering only adds a background fill to cells that are already
//!   painted (or padding to the row's existing width), so rows, columns, and
//!   hit regions stay exactly where a flat surface puts them.
//! * Motion sets the activity spinner cadence and whether a `sweep` focus
//!   gradient advances. Animation piggybacks on the activity tick, which is
//!   only scheduled while activity is shown, so an idle UI never schedules
//!   frames. `[tui] reduce_motion = true` forces `none`.
//! * Motion also sets the durations of accent transitions, transcript scroll
//!   animations, and the latest-bar glow. `none` makes all three instant, so
//!   they schedule no frames. An explicit accent transition setting wins over
//!   the theme's level; `reduce_motion` wins over everything.

use std::time::Duration;

use bcode_tui_theme::chrome::{FocusEffect, IconSet, Motion, SeparatorStyle, SurfaceStyle};
use bcode_tui_theme::definition::{
    ContainerBorder, ContainerLayout, ContainerRecipe, ContainerWidth, ResolvedThemeDefinition,
};
use bmux_tui::gradient::PHASE_CYCLE;
use bmux_tui::prelude::{Line, Span, Style};
use bmux_tui::style::Color;
use bmux_tui::text_width::display_width;

/// Decorative chrome of a resolved theme.
///
/// The default reproduces a theme without `[chrome.tui]`: plain separators,
/// flat surfaces, and subtle motion (the activity cadence every theme used
/// before motion was configurable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ChromeEffects {
    /// Icon set; selects the separator fallback chain.
    pub icons: IconSet,
    /// Status line separator style.
    pub separators: SeparatorStyle,
    /// Surface layering for tool units and group headers.
    pub surfaces: SurfaceStyle,
    /// Theme motion level, before `reduce_motion`.
    pub motion: Motion,
    /// Whether the theme asks for a sweeping focus gradient it can draw
    /// (the `sweep` effect with two RGB gradient colors).
    pub focus_sweep: bool,
}

/// Separator candidates, most specific first: Nerd Font, Unicode, ASCII.
struct SeparatorCandidates {
    nerd: &'static str,
    unicode: &'static str,
    ascii: &'static str,
}

/// Width every status separator occupies: one glyph cell between two spaces.
pub const SEPARATOR_CELLS: usize = 3;

/// The established middle-dot separator; `plain` keeps it for every icon set.
const PLAIN_SEPARATOR: &str = " · ";

const fn separator_candidates(style: SeparatorStyle) -> SeparatorCandidates {
    match style {
        SeparatorStyle::Plain => SeparatorCandidates {
            nerd: PLAIN_SEPARATOR,
            unicode: PLAIN_SEPARATOR,
            ascii: PLAIN_SEPARATOR,
        },
        SeparatorStyle::Bar => SeparatorCandidates {
            nerd: " │ ",
            unicode: " │ ",
            ascii: " | ",
        },
        // Powerline-extra thin forward slash.
        SeparatorStyle::Slant => SeparatorCandidates {
            nerd: " \u{e0bb} ",
            unicode: " ╱ ",
            ascii: " / ",
        },
        // Powerline thin right arrow: segments carry no background of their
        // own, so the filled arrow would have no colors to join.
        SeparatorStyle::Powerline => SeparatorCandidates {
            nerd: " \u{e0b1} ",
            unicode: " ❯ ",
            ascii: " > ",
        },
    }
}

/// First candidate BMUX measures at exactly [`SEPARATOR_CELLS`] cells.
fn measured_separator<'a>(candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| display_width(candidate) == SEPARATOR_CELLS)
}

/// Steps the focus gradient phase advances per activity tick under `subtle`
/// (a full cycle over 100 ticks) and `full` (a full cycle over 40 ticks).
const SUBTLE_SWEEP_STEP: u16 = 20;
const FULL_SWEEP_STEP: u16 = 50;

/// Activity tick cadence per motion level. `none` still ticks, slowly, so
/// the elapsed-time label stays current while the spinner glyph holds still.
const STILL_ACTIVITY_FRAME: Duration = Duration::from_secs(1);
const SUBTLE_ACTIVITY_FRAME: Duration = Duration::from_millis(100);
const FULL_ACTIVITY_FRAME: Duration = Duration::from_millis(80);

/// Transition durations under `subtle` (the durations every theme used before
/// motion governed them) and `full` (half again as long, so motion is more
/// visible). `none` makes every transition instant.
const SUBTLE_ACCENT_TRANSITION: Duration = Duration::from_millis(220);
const FULL_ACCENT_TRANSITION: Duration = Duration::from_millis(330);
const SUBTLE_TRANSCRIPT_SCROLL: Duration = Duration::from_millis(180);
const FULL_TRANSCRIPT_SCROLL: Duration = Duration::from_millis(270);
const SUBTLE_LATEST_BAR_WINDOW: Duration = Duration::from_millis(420);
const FULL_LATEST_BAR_WINDOW: Duration = Duration::from_millis(630);

impl ChromeEffects {
    /// Decorative chrome of a resolved theme definition; `None` yields the default.
    #[must_use]
    pub fn from_theme(theme: Option<&ResolvedThemeDefinition>) -> Self {
        theme.map_or_else(Self::default, |theme| Self {
            icons: theme.chrome.icons,
            separators: theme.chrome.separators,
            surfaces: theme.chrome.surfaces,
            motion: theme.chrome.motion,
            focus_sweep: theme.chrome.focus == FocusEffect::Sweep
                && super::chrome::focus_gradient(&theme.chrome).is_some(),
        })
    }

    /// The text joining status line segments, always
    /// [`SEPARATOR_CELLS`] cells wide.
    ///
    /// `none` icon sets use the ASCII candidate, `unicode` the Unicode then
    /// ASCII candidates, and `nerd` the Nerd Font glyph first. A candidate
    /// whose measured width is not [`SEPARATOR_CELLS`] falls back to the
    /// next, more portable one.
    #[must_use]
    pub fn status_separator(self) -> &'static str {
        let candidates = separator_candidates(self.separators);
        let chain: &[&'static str] = match self.icons {
            IconSet::None => &[candidates.ascii],
            IconSet::Unicode => &[candidates.unicode, candidates.ascii],
            IconSet::Nerd => &[candidates.nerd, candidates.unicode, candidates.ascii],
        };
        measured_separator(chain).unwrap_or(PLAIN_SEPARATOR)
    }

    /// Whether tool units and group headers sit on the raised surface.
    #[must_use]
    pub const fn layered(self) -> bool {
        matches!(self.surfaces, SurfaceStyle::Layered)
    }

    /// Background a layered surface paints, when it differs from the canvas.
    ///
    /// `None` for flat surfaces and for themes whose raised surface has no
    /// background of its own, so layering never repaints cells to no effect.
    #[must_use]
    pub fn raised_background(self, raised: Style, canvas: Style) -> Option<Color> {
        if !self.layered() {
            return None;
        }
        raised.bg.filter(|bg| canvas.bg != Some(*bg))
    }

    /// Container recipe and style of a tool unit on this surface.
    ///
    /// * flat (or no raised background): unchanged;
    /// * a recipe with a container: its style gains the raised background
    ///   where it has none, so padding and unstyled content share the fill;
    /// * a plain recipe: a borderless, unpadded full-width panel whose style
    ///   is only the raised background. Its content column and width equal
    ///   the plain layout's, so geometry and hit regions are unchanged.
    #[must_use]
    pub const fn tool_surface(
        recipe: ContainerRecipe,
        style: Style,
        raised: Option<Color>,
    ) -> (ContainerRecipe, Style) {
        let Some(raised) = raised else {
            return (recipe, style);
        };
        if matches!(recipe.layout, ContainerLayout::Plain) {
            let panel = ContainerRecipe {
                layout: ContainerLayout::Panel,
                width: ContainerWidth::Full,
                border: ContainerBorder::None,
                padding_x: 0,
                padding_y: 0,
            };
            return (panel, Style::new().bg(raised));
        }
        let style = if style.bg.is_some() {
            style
        } else {
            style.bg(raised)
        };
        (recipe, style)
    }
}

/// `line` on a raised surface `width` cells wide.
///
/// Spans without a background take `raised`, and the row is padded with
/// raised blanks up to `width`, measured by BMUX. Content is never moved or
/// clipped: a line already at or past `width` only gains the fill.
#[must_use]
pub fn raised_row(line: &Line, width: usize, raised: Color) -> Line {
    let fill = Style::new().bg(raised);
    let mut row = line.with_fallback_style(fill);
    let used = row.width();
    if used < width {
        row.push_span(Span::styled(" ".repeat(width - used), fill));
    }
    row
}

/// Effective motion: the theme's level, unless `reduce_motion` forces `none`.
///
/// Precedence for every animation, highest first:
///
/// * `[tui] reduce_motion = true`: instant, whatever else is set;
/// * an explicit user setting for that animation (only accent transitions
///   have one: `[tui.theme] accent_transition` and `accent_transition_ms`);
/// * the theme's `motion` level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotionPolicy {
    motion: Motion,
    focus_sweep: bool,
    reduced: bool,
}

impl MotionPolicy {
    /// Motion for `effects` under the `reduce_motion` setting.
    #[must_use]
    pub const fn new(effects: ChromeEffects, reduce_motion: bool) -> Self {
        Self {
            motion: if reduce_motion {
                Motion::None
            } else {
                effects.motion
            },
            focus_sweep: effects.focus_sweep,
            reduced: reduce_motion,
        }
    }

    /// Picks the `subtle` or `full` duration; `none` is instant.
    const fn level_duration(self, subtle: Duration, full: Duration) -> Duration {
        match self.motion {
            Motion::None => Duration::ZERO,
            Motion::Subtle => subtle,
            Motion::Full => full,
        }
    }

    /// Duration of an accent color transition.
    ///
    /// `explicit` is the user's own setting (`Some(ZERO)` for
    /// `accent_transition = "immediate"`), which wins over the theme's level;
    /// `reduce_motion` makes every transition instant.
    #[must_use]
    pub const fn accent_transition(self, explicit: Option<Duration>) -> Duration {
        if self.reduced {
            return Duration::ZERO;
        }
        match explicit {
            Some(duration) => duration,
            None => self.level_duration(SUBTLE_ACCENT_TRANSITION, FULL_ACCENT_TRANSITION),
        }
    }

    /// Duration of an animated transcript scroll; zero jumps immediately.
    #[must_use]
    pub const fn transcript_scroll(self) -> Duration {
        self.level_duration(SUBTLE_TRANSCRIPT_SCROLL, FULL_TRANSCRIPT_SCROLL)
    }

    /// How long the latest-content bar glows after hidden activity; zero
    /// shows the still bar immediately.
    #[must_use]
    pub const fn latest_bar_window(self) -> Duration {
        self.level_duration(SUBTLE_LATEST_BAR_WINDOW, FULL_LATEST_BAR_WINDOW)
    }

    /// Effective motion level.
    #[must_use]
    pub const fn motion(self) -> Motion {
        self.motion
    }

    /// Interval between activity ticks while activity is shown.
    #[must_use]
    pub const fn activity_frame_interval(self) -> Duration {
        match self.motion {
            Motion::None => STILL_ACTIVITY_FRAME,
            Motion::Subtle => SUBTLE_ACTIVITY_FRAME,
            Motion::Full => FULL_ACTIVITY_FRAME,
        }
    }

    /// Spinner step for the `activity_frame`th tick: `none` holds the first
    /// frame, other levels advance one frame per tick.
    #[must_use]
    pub const fn spinner_step(self, activity_frame: usize) -> usize {
        match self.motion {
            Motion::None => 0,
            Motion::Subtle | Motion::Full => activity_frame,
        }
    }

    /// Whether the focus gradient sweeps on activity ticks.
    #[must_use]
    pub const fn focus_sweeps(self) -> bool {
        self.focus_sweep && !matches!(self.motion, Motion::None)
    }

    /// Focus gradient phase for the `activity_frame`th tick, in
    /// `0..PHASE_CYCLE`; always `0` when the gradient does not sweep.
    #[must_use]
    pub fn focus_phase(self, activity_frame: usize) -> u16 {
        if !self.focus_sweeps() {
            return 0;
        }
        let step = match self.motion {
            Motion::Full => FULL_SWEEP_STEP,
            Motion::None | Motion::Subtle => SUBTLE_SWEEP_STEP,
        };
        let ticks_per_cycle = PHASE_CYCLE / step;
        // The remainder is below `ticks_per_cycle`, so it always fits.
        u16::try_from(activity_frame % usize::from(ticks_per_cycle)).map_or(0, |tick| tick * step)
    }
}

#[cfg(test)]
mod tests {
    use bcode_tui_theme::chrome::TuiChrome;
    use bcode_tui_theme::definition::{ContainerLayout, ResolvedThemeDefinition};

    use super::*;

    fn effects(icons: IconSet, separators: SeparatorStyle) -> ChromeEffects {
        ChromeEffects {
            icons,
            separators,
            ..ChromeEffects::default()
        }
    }

    const ALL_SEPARATORS: [SeparatorStyle; 4] = [
        SeparatorStyle::Plain,
        SeparatorStyle::Bar,
        SeparatorStyle::Slant,
        SeparatorStyle::Powerline,
    ];
    const ALL_ICONS: [IconSet; 3] = [IconSet::None, IconSet::Unicode, IconSet::Nerd];

    #[test]
    fn themes_without_chrome_keep_the_established_effects() {
        let theme = ResolvedThemeDefinition::empty("plain", "Plain");
        let defaults = ChromeEffects::from_theme(Some(&theme));
        assert_eq!(defaults, ChromeEffects::default());
        assert_eq!(ChromeEffects::from_theme(None), defaults);
        assert_eq!(defaults.status_separator(), " · ");
        assert!(!defaults.layered());
        let motion = MotionPolicy::new(defaults, false);
        assert_eq!(motion.motion(), Motion::Subtle);
        assert_eq!(motion.activity_frame_interval(), Duration::from_millis(100));
        assert_eq!(motion.spinner_step(7), 7);
        assert!(!motion.focus_sweeps());
    }

    #[test]
    fn separators_keep_a_constant_measured_width_for_every_icon_set() {
        for separators in ALL_SEPARATORS {
            for icons in ALL_ICONS {
                let separator = effects(icons, separators).status_separator();
                assert_eq!(
                    display_width(separator),
                    SEPARATOR_CELLS,
                    "{separators:?}/{icons:?}: {separator:?}"
                );
            }
        }
    }

    #[test]
    fn font_dependent_separators_follow_icon_fallback_rules() {
        for separators in [SeparatorStyle::Slant, SeparatorStyle::Powerline] {
            let none = effects(IconSet::None, separators).status_separator();
            assert!(none.is_ascii(), "{separators:?}: {none:?}");
            let unicode = effects(IconSet::Unicode, separators).status_separator();
            assert!(
                !unicode
                    .chars()
                    .any(|glyph| ('\u{e000}'..='\u{f8ff}').contains(&glyph)),
                "unicode sets never use private-use glyphs: {unicode:?}"
            );
            let nerd = effects(IconSet::Nerd, separators).status_separator();
            assert_ne!(nerd, unicode, "nerd sets use the powerline glyph");
        }
        assert_eq!(
            effects(IconSet::None, SeparatorStyle::Bar).status_separator(),
            " | "
        );
        assert_eq!(
            effects(IconSet::None, SeparatorStyle::Plain).status_separator(),
            " · "
        );
    }

    #[test]
    fn unmeasurable_separators_fall_back_to_portable_candidates() {
        // A lone combining mark measures one cell short; a wide glyph one long.
        assert_eq!(
            measured_separator(&[" \u{301} ", " ╱ ", " / "]),
            Some(" ╱ ")
        );
        assert_eq!(measured_separator(&[" 漢 ", " / "]), Some(" / "));
        assert_eq!(measured_separator(&[" 👍 "]), None);
    }

    #[test]
    fn theme_chrome_selects_separators_surfaces_and_motion() {
        let mut theme = ResolvedThemeDefinition::empty("riced", "Riced");
        theme.chrome = TuiChrome {
            icons: IconSet::Nerd,
            separators: SeparatorStyle::Powerline,
            surfaces: SurfaceStyle::Layered,
            motion: Motion::Full,
            focus: FocusEffect::Sweep,
            focus_colors: vec![Color::Rgb(255, 0, 170), Color::Rgb(0, 255, 255)],
            ..TuiChrome::default()
        };
        let effects = ChromeEffects::from_theme(Some(&theme));
        assert!(effects.layered());
        assert!(effects.focus_sweep);
        assert_eq!(effects.status_separator(), " \u{e0b1} ");
        theme.chrome.focus_colors = vec![Color::Cyan, Color::Magenta];
        assert!(
            !ChromeEffects::from_theme(Some(&theme)).focus_sweep,
            "terminal-owned colors cannot form a gradient"
        );
    }

    #[test]
    fn reduce_motion_forces_still_spinners_and_focus() {
        let effects = ChromeEffects {
            motion: Motion::Full,
            focus_sweep: true,
            ..ChromeEffects::default()
        };
        let full = MotionPolicy::new(effects, false);
        assert_eq!(full.motion(), Motion::Full);
        assert!(full.focus_sweeps());
        assert!(full.activity_frame_interval() < SUBTLE_ACTIVITY_FRAME);
        assert_eq!(full.spinner_step(3), 3);
        assert_ne!(full.focus_phase(1), full.focus_phase(2));

        let reduced = MotionPolicy::new(effects, true);
        assert_eq!(reduced.motion(), Motion::None);
        assert!(!reduced.focus_sweeps());
        assert_eq!(reduced.spinner_step(3), reduced.spinner_step(4));
        assert_eq!(reduced.focus_phase(9), 0);
        assert_eq!(reduced.activity_frame_interval(), Duration::from_secs(1));
    }

    #[test]
    fn focus_phase_cycles_within_the_gradient_range() {
        for motion in [Motion::Subtle, Motion::Full] {
            let policy = MotionPolicy::new(
                ChromeEffects {
                    motion,
                    focus_sweep: true,
                    ..ChromeEffects::default()
                },
                false,
            );
            let phases = (0..500).map(|frame| policy.focus_phase(frame));
            assert!(phases.clone().all(|phase| phase < PHASE_CYCLE));
            assert_eq!(policy.focus_phase(0), 0);
            assert_eq!(
                policy.focus_phase(usize::MAX),
                policy.focus_phase(usize::MAX)
            );
            assert!(phases.clone().any(|phase| phase > 0));
        }
        let subtle = MotionPolicy::new(
            ChromeEffects {
                focus_sweep: true,
                ..ChromeEffects::default()
            },
            false,
        );
        let full = MotionPolicy::new(
            ChromeEffects {
                motion: Motion::Full,
                focus_sweep: true,
                ..ChromeEffects::default()
            },
            false,
        );
        assert!(full.focus_phase(1) > subtle.focus_phase(1));
    }

    fn policy(motion: Motion, reduce_motion: bool) -> MotionPolicy {
        MotionPolicy::new(
            ChromeEffects {
                motion,
                ..ChromeEffects::default()
            },
            reduce_motion,
        )
    }

    #[test]
    fn motion_levels_govern_transition_durations() {
        let none = policy(Motion::None, false);
        let subtle = policy(Motion::Subtle, false);
        let full = policy(Motion::Full, false);
        for still in [none, policy(Motion::Full, true)] {
            assert_eq!(still.accent_transition(None), Duration::ZERO);
            assert_eq!(still.transcript_scroll(), Duration::ZERO);
            assert_eq!(still.latest_bar_window(), Duration::ZERO);
        }
        // `subtle` keeps the durations used before motion governed them.
        assert_eq!(subtle.accent_transition(None), Duration::from_millis(220));
        assert_eq!(subtle.transcript_scroll(), Duration::from_millis(180));
        assert_eq!(subtle.latest_bar_window(), Duration::from_millis(420));
        assert!(full.accent_transition(None) > subtle.accent_transition(None));
        assert!(full.transcript_scroll() > subtle.transcript_scroll());
        assert!(full.latest_bar_window() > subtle.latest_bar_window());
    }

    #[test]
    fn explicit_accent_settings_beat_theme_motion_but_not_reduce_motion() {
        let explicit = Some(Duration::from_millis(500));
        for motion in [Motion::None, Motion::Subtle, Motion::Full] {
            assert_eq!(
                policy(motion, false).accent_transition(explicit),
                Duration::from_millis(500),
                "{motion:?}"
            );
            assert_eq!(
                policy(motion, false).accent_transition(Some(Duration::ZERO)),
                Duration::ZERO,
                "immediate stays instant under {motion:?}"
            );
            assert_eq!(
                policy(motion, true).accent_transition(explicit),
                Duration::ZERO,
                "reduce_motion wins under {motion:?}"
            );
        }
    }

    fn recipe(layout: ContainerLayout, border: ContainerBorder) -> ContainerRecipe {
        ContainerRecipe {
            layout,
            width: ContainerWidth::Content,
            border,
            padding_x: 1,
            padding_y: 0,
        }
    }

    #[test]
    fn flat_surfaces_and_canvas_colored_raised_surfaces_paint_nothing() {
        let raised = Style::new().bg(Color::Rgb(30, 30, 40));
        let canvas = Style::new().bg(Color::Rgb(20, 20, 20));
        let flat = ChromeEffects::default();
        assert_eq!(flat.raised_background(raised, canvas), None);
        let layered = ChromeEffects {
            surfaces: SurfaceStyle::Layered,
            ..ChromeEffects::default()
        };
        assert_eq!(
            layered.raised_background(raised, canvas),
            Some(Color::Rgb(30, 30, 40))
        );
        assert_eq!(layered.raised_background(canvas, canvas), None);
        assert_eq!(layered.raised_background(Style::new(), canvas), None);
        let plain = recipe(ContainerLayout::Plain, ContainerBorder::None);
        let style = Style::new().fg(Color::Green);
        assert_eq!(
            ChromeEffects::tool_surface(plain, style, None),
            (plain, style)
        );
    }

    #[test]
    fn layered_tool_surfaces_only_add_a_background() {
        let raised = Color::Rgb(30, 30, 40);
        let bar = recipe(ContainerLayout::LeftBar, ContainerBorder::Left);
        let green = Style::new().fg(Color::Green);
        let (recipe_out, style) = ChromeEffects::tool_surface(bar, green, Some(raised));
        assert_eq!(recipe_out, bar);
        assert_eq!(style, green.bg(raised));
        let own_bg = green.bg(Color::Red);
        assert_eq!(
            ChromeEffects::tool_surface(bar, own_bg, Some(raised)).1,
            own_bg,
            "an authored container background wins"
        );
        let (panel, fill) = ChromeEffects::tool_surface(
            recipe(ContainerLayout::Plain, ContainerBorder::None),
            green,
            Some(raised),
        );
        assert_eq!(panel.layout, ContainerLayout::Panel);
        assert_eq!(panel.border, ContainerBorder::None);
        assert_eq!((panel.padding_x, panel.padding_y), (0, 0));
        assert_eq!(
            fill,
            Style::new().bg(raised),
            "no foreground leaks into content"
        );
    }

    #[test]
    fn raised_rows_fill_to_width_without_moving_content() {
        let raised = Color::Rgb(30, 30, 40);
        for (text, width) in [
            ("abc", 8_usize),
            ("漢字", 6),
            ("👍 ok", 7),
            ("e\u{301}x", 4),
            ("", 0),
            ("", 3),
            ("longer than width", 4),
        ] {
            let line = Line::from_spans(vec![Span::styled(text, Style::new().fg(Color::Yellow))]);
            let row = raised_row(&line, width, raised);
            assert_eq!(row.width(), line.width().max(width), "{text:?}@{width}");
            assert!(row.plain_text().starts_with(text));
            assert!(
                row.spans
                    .iter()
                    .filter(|span| !span.content.is_empty())
                    .all(|span| span.style.bg == Some(raised))
            );
            assert_eq!(row.spans[0].style.fg, Some(Color::Yellow));
        }
        let own = Line::from_spans(vec![Span::styled("x", Style::new().bg(Color::Red))]);
        assert_eq!(
            raised_row(&own, 1, raised).spans[0].style.bg,
            Some(Color::Red)
        );
    }
}
