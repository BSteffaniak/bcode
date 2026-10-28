//! Editorial presentation assembled from locked BMUX components.
use super::{App, Screen, Stage};
use bcode_recap::contracts::{BlockContent, RecapDocument};
use bmux_tui::{
    component::{Element, LogicalInsets},
    composition::{Column, Flex, Row, Surface, TextBlock},
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use bmux_tui_components::{
    button::{ButtonComponent, ButtonStyles},
    scrollbar::ScrollbarStyles,
    text_input::TextInputPolicy,
    text_input_box::{TextInputBoxComponent, TextInputBoxPolicy, TextInputBoxStyles},
    text_view::{TextViewComponent, TextViewStyles},
};

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub base: Color,
    panel: Color,
    raised: Color,
    pub ink: Color,
    muted: Color,
    accent: Color,
    violet: Color,
    variant: u8,
}
impl Palette {
    pub(super) const fn dark() -> Self {
        Self {
            base: Color::Rgb(17, 21, 27),
            panel: Color::Rgb(25, 31, 39),
            raised: Color::Rgb(35, 43, 52),
            ink: Color::Rgb(237, 231, 219),
            muted: Color::Rgb(151, 163, 173),
            accent: Color::Rgb(104, 218, 196),
            violet: Color::Rgb(184, 163, 242),
            variant: 0,
        }
    }
    pub(super) const fn monochrome() -> Self {
        Self {
            base: Color::Black,
            panel: Color::Black,
            raised: Color::Black,
            ink: Color::White,
            muted: Color::White,
            accent: Color::White,
            violet: Color::White,
            variant: 2,
        }
    }
    pub(super) const fn next(self) -> Self {
        match self.variant {
            0 => Self {
                base: Color::Rgb(243, 241, 235),
                panel: Color::Rgb(232, 231, 225),
                raised: Color::Rgb(215, 225, 221),
                ink: Color::Rgb(32, 40, 46),
                muted: Color::Rgb(82, 97, 105),
                accent: Color::Rgb(0, 103, 93),
                violet: Color::Rgb(102, 68, 152),
                variant: 1,
            },
            1 => Self {
                base: Color::Black,
                panel: Color::Black,
                raised: Color::Black,
                ink: Color::White,
                muted: Color::White,
                accent: Color::Cyan,
                violet: Color::Magenta,
                variant: 2,
            },
            _ => Self::dark(),
        }
    }
    const fn text(self) -> Style {
        Style::new().fg(self.ink)
    }
    const fn quiet(self) -> Style {
        Style::new().fg(self.muted)
    }
    const fn title(self) -> Style {
        Style::new().fg(self.ink).add_modifier(Modifier::BOLD)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Density {
    Tiny,
    Narrow,
    Medium,
    Wide,
}
const fn density(width: u16, height: u16) -> Density {
    if width < 34 || height < 13 {
        Density::Tiny
    } else if width < 76 {
        Density::Narrow
    } else if width < 112 {
        Density::Medium
    } else {
        Density::Wide
    }
}

impl App {
    fn label(text: impl Into<String>, style: Style) -> TextBlock {
        TextBlock::new(text.into()).style(style)
    }
    fn button<'a>(&'a self, id: &str, label: &'a str, index: usize) -> ButtonComponent<'a, 'a> {
        let p = self.palette;
        ButtonComponent::new(id.to_owned(), label, &self.buttons[index])
            .filled()
            .styles(ButtonStyles {
                normal: p.text().bg(p.raised),
                focused: Style::new().fg(p.base).bg(p.accent),
                hovered: Style::new().fg(p.base).bg(p.accent),
                pressed: Style::new().fg(p.base).bg(p.violet),
                disabled: p.quiet(),
            })
    }
    fn field<'a>(
        &'a self,
        index: usize,
        label: &'a str,
        placeholder: &'a str,
    ) -> TextInputBoxComponent<'a, 'a> {
        let p = self.palette;
        let mut policy = TextInputBoxPolicy::labeled_field();
        policy.focused = self.focused == index;
        policy.max_rows = Some(1);
        policy.min_rows = 1;
        let mut text_policy = TextInputPolicy::chat_composer();
        text_policy.viewport.max_rows = Some(1);
        TextInputBoxComponent::new(
            super::FIELDS[index].trim_end_matches(".input"),
            text_policy,
            &self.fields[index],
        )
        .label(label)
        .placeholder(placeholder)
        .policy(policy)
        .styles(TextInputBoxStyles {
            text: p.text().bg(if self.focused == index {
                p.raised
            } else {
                p.panel
            }),
            focused_text: p.text().bg(p.raised),
            disabled_text: p.quiet().bg(p.panel),
            placeholder: p.quiet().bg(if self.focused == index {
                p.raised
            } else {
                p.panel
            }),
            selection: Style::new().fg(p.base).bg(p.accent),
            border: p.quiet(),
            focused_border: Style::new().fg(p.accent),
            background: Style::new().bg(p.panel),
            focused_background: Style::new().bg(p.raised),
            disabled_background: Style::new().bg(p.panel),
        })
    }
    pub(super) fn tree(&self) -> Element<'_> {
        let (width, height) = self.size.get();
        let mode = density(width, height);
        let p = self.palette;
        if mode == Density::Tiny {
            return Element::new(
                Surface::new(
                    Column::new()
                        .child(Self::label(
                            "RECAP",
                            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
                        ))
                        .child(Self::label(
                            "A little more room, a better perspective.",
                            p.text(),
                        ))
                        .child(Self::label(
                            "Resize to 34 × 13 or larger. F10 / Ctrl+C closes safely.",
                            p.quiet(),
                        ))
                        .child(Self::label(
                            if self.active.is_some() {
                                "Operation still running · Esc requests cancellation"
                            } else {
                                "No model runs just by opening Recap."
                            },
                            p.quiet(),
                        )),
                )
                .background(Style::new().bg(p.base))
                .content_style(p.text().bg(p.base)),
            );
        }
        let header = Row::new()
            .gap(2)
            .child(Self::label(
                "◈  R E C A P",
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
            ))
            .flex_child(1, Self::label("  A clearer view.", p.quiet()))
            .child(self.button("home", "Home", 0))
            .child(self.button("theme", "◐", 1));
        let body = if self.active.is_some() {
            self.working()
        } else {
            match self.screen {
                Screen::Home => self.home(mode),
                Screen::Reader => self.reader(mode),
            }
        };
        let footer = Column::new()
            .child(Self::label(self.status.clone(), p.quiet()))
            .child(Self::label(
                "F2 Generate   F3 Chapters   F4 Focus   F5 Export   F6 Open   F8 Theme   F10/Ctrl+C Quit",
                Style::new().fg(p.accent),
            ));
        Element::new(
            Surface::new(
                Column::new()
                    .gap(1)
                    .child(header)
                    .flex(Flex::new(1, body))
                    .child(footer),
            )
            .background(Style::new().bg(p.base))
            .content_style(p.text().bg(p.base))
            .padding(LogicalInsets::new(1, 2, 0, 2)),
        )
    }
    fn home(&self, mode: Density) -> Element<'_> {
        let p = self.palette;
        if self.drawer {
            return self.library();
        }
        let mut form = Column::new()
            .child(Self::label(
                "MAKE SOMETHING WORTH KEEPING",
                Style::new().fg(p.violet),
            ))
            .child(Self::label("From video to perspective.", p.title()))
            .child(Self::label(
                "A source-grounded visual recap, made to revisit.",
                p.quiet(),
            ))
            .child(self.field(0, "SOURCE", "YouTube URL or local video path"));
        if self.refine {
            form = form
                .child(self.field(1, "TRANSCRIPT · OPTIONAL", "Local VTT or SRT file"))
                .child(self.field(
                    2,
                    "EDITORIAL FOCUS · OPTIONAL",
                    "What should this recap bring into focus?",
                ));
        } else {
            form = form.child(
                Row::new()
                    .gap(1)
                    .flex_child(
                        1,
                        self.field(3, "MODEL CONFIG", "Explicit Bcode config path"),
                    )
                    .flex_child(1, self.field(4, "PROFILE", "Profile name")),
            );
        }
        form = form.child(
            Row::new()
                .gap(1)
                .child(self.button("generate", "Generate  ↗", 2))
                .child(self.button(
                    "options",
                    if self.refine {
                        "Model settings"
                    } else {
                        "Source options"
                    },
                    8,
                ))
                .child(self.button(
                    "depth",
                    match self.depth {
                        bcode_recap::contracts::Depth::Brief => "Brief",
                        bcode_recap::contracts::Depth::Standard => "Standard",
                        bcode_recap::contracts::Depth::Detailed => "Detailed",
                    },
                    3,
                )),
        );
        let form = Surface::new(form)
            .background(Style::new().bg(p.panel))
            .content_style(p.text().bg(p.panel))
            .padding(LogicalInsets::all(1));
        if mode == Density::Wide {
            Element::new(
                Row::new()
                    .gap(2)
                    .flex_child(3, form)
                    .flex_child(2, self.library()),
            )
        } else {
            Element::new(form)
        }
    }
    fn library(&self) -> Element<'_> {
        let p = self.palette;
        let mut library = Column::new()
            .gap(1)
            .child(Self::label("YOUR LIBRARY", Style::new().fg(p.violet)))
            .child(Self::label("Good ideas deserve a second look.", p.title()));
        if self.recent.is_empty() {
            library = library.child(Self::label(
                "No recaps on this page yet.\nCreate one from a source above.",
                p.quiet(),
            ));
        }
        for (index, entry) in self
            .recent
            .iter()
            .enumerate()
            .skip(self.library_offset)
            .take(6)
        {
            library = library.child(self.button(
                &format!("recent-{index}"),
                &entry.title,
                index % 6 + 10,
            ));
            if let Some(revision) = entry.revision {
                library = library.child(Self::label(format!("Revision {revision}"), p.quiet()));
            }
            if entry.error.is_some() {
                library = library.child(Self::label(
                    "Needs attention · could not validate this document",
                    Style::new().fg(p.violet),
                ));
            }
        }
        if self.library_offset > 0 {
            library = library.child(self.button("library-prev", "← Previous recaps", 75));
        }
        if self.library_offset + 6 < self.recent.len() {
            library = library.child(self.button("library-next", "More recaps →", 76));
        }
        if self.cursor.is_some() {
            library = library.child(self.button("more", "Next library page  →", 5));
        }
        Element::new(
            Surface::new(library)
                .background(Style::new().bg(p.panel))
                .content_style(p.text().bg(p.panel))
                .padding(LogicalInsets::all(1)),
        )
    }
    fn working(&self) -> Element<'_> {
        let p = self.palette;
        let active = self
            .active
            .as_ref()
            .expect("working screen has an operation");
        let stage = active.stage;
        let steps = [
            (Stage::ModelReadiness, "00  Model readiness"),
            (Stage::Acquisition, "01  Source & captions"),
            (Stage::Media, "02  Source visuals"),
            (Stage::Composition, "03  Compose perspective"),
            (Stage::Storage, "04  Validate & save"),
        ];
        let mut body = Column::new()
            .gap(1)
            .child(Self::label(
                if active.cancelling {
                    "FINISHING SAFELY"
                } else {
                    "A NEW PERSPECTIVE IS TAKING SHAPE"
                },
                Style::new().fg(p.violet),
            ))
            .child(Self::label(
                if active.cancelling {
                    "Cancellation requested."
                } else {
                    "Good understanding takes a moment."
                },
                p.title(),
            ))
            .child(Self::label(
                format!(
                    "Elapsed {}s · {}",
                    active.started.elapsed().as_secs(),
                    if stage.is_none() {
                        "Checking readiness"
                    } else {
                        "Source-grounded pipeline"
                    }
                ),
                p.quiet(),
            ));
        for (current, label) in steps {
            body = body.child(Self::label(
                format!(
                    "{}  {label}",
                    if stage == Some(current) { "◉" } else { "·" }
                ),
                if stage == Some(current) {
                    Style::new().fg(p.accent)
                } else {
                    p.quiet()
                },
            ));
        }
        body = body
            .child(Self::label(
                "Stages report real engine activity. No estimated percentage.",
                p.quiet(),
            ))
            .child(self.button(
                "cancel",
                if active.cancelling {
                    "Waiting for settlement…"
                } else {
                    "Cancel operation"
                },
                6,
            ));
        Element::new(
            Surface::new(body)
                .background(Style::new().bg(p.panel))
                .content_style(p.text().bg(p.panel))
                .padding(LogicalInsets::all(2)),
        )
    }
    fn reader_content(&self) -> Element<'_> {
        let p = self.palette;
        let reading =
            TextViewComponent::new("reader", &self.lines, &self.scroll).styles(TextViewStyles {
                text: p.text(),
                empty: p.quiet(),
                background: Style::new().bg(p.panel),
                scrollbar: ScrollbarStyles::default(),
            });
        let mut reader_body = Column::new();
        if !self.image_capabilities.any_supported() {
            reader_body=reader_body.child(Self::label("SOURCE IMAGES · captions shown here; this terminal does not advertise image support",p.quiet()));
        }
        if let Some(preview) = &self.preview {
            reader_body = reader_body.child(preview.clone());
        }
        reader_body = reader_body.flex(Flex::new(1, reading));
        let content = Surface::new(reader_body)
            .background(Style::new().bg(p.panel))
            .content_style(p.text().bg(p.panel))
            .padding(LogicalInsets::all(1));
        Element::new(content)
    }
    fn reader(&self, mode: Density) -> Element<'_> {
        let p = self.palette;
        let Some(document) = &self.document else {
            return Element::new(Self::label("Choose a recap from Home.", p.quiet()));
        };
        let actions = Row::new()
            .gap(1)
            .child(self.button("chapters", "Chapters", 7))
            .child(self.button("refine", "Refine focus", 8))
            .child(self.button("export", "Export HTML", 9))
            .child(self.button("open", "Open HTML", 4));
        let content = self.reader_content();
        let mut chapters = Column::new()
            .gap(1)
            .child(Self::label("CONTENTS", Style::new().fg(p.violet)))
            .child(Self::label(
                format!(
                    "{} {} · revision {}",
                    document.sections.len(),
                    if document.sections.len() == 1 {
                        "chapter"
                    } else {
                        "chapters"
                    },
                    document.revision
                ),
                p.quiet(),
            ));
        for (index, section) in document
            .sections
            .iter()
            .enumerate()
            .skip(self.chapter_offset)
            .take(8)
        {
            chapters = chapters.child(self.button(
                &format!("chapter-{index}"),
                &section.title,
                index % 8 + 40,
            ));
        }
        if self.chapter_offset > 0 {
            chapters = chapters.child(self.button("chapter-prev", "← Earlier chapters", 77));
        }
        if self.chapter_offset + 8 < document.sections.len() {
            chapters = chapters.child(self.button("chapter-next", "Later chapters →", 78));
        }
        let middle = if mode == Density::Wide {
            Element::new(
                Row::new()
                    .gap(2)
                    .flex_child(1, Surface::new(chapters).padding(LogicalInsets::all(1)))
                    .flex_child(3, content),
            )
        } else if self.drawer {
            Element::new(
                Surface::new(chapters)
                    .background(Style::new().bg(p.panel))
                    .content_style(p.text().bg(p.panel))
                    .padding(LogicalInsets::all(1)),
            )
        } else {
            Element::new(content)
        };
        let mut body = Column::new()
            .gap(1)
            .child(Self::label(document.title.clone(), p.title()))
            .child(actions)
            .flex(Flex::new(1, middle));
        if self.refine {
            body = body
                .child(self.field(
                    2,
                    "REFINE THIS RECAP",
                    "A new editorial focus; explicit model call",
                ))
                .child(self.button("apply", "Apply focus  ↗", 74));
        }
        body = body.child(self.field(
            5,
            "EXPORT DESTINATION · OPTIONAL",
            "Defaults to <recap-id>.html; existing files are not overwritten",
        ));
        Element::new(body)
    }
}

pub(super) fn reader_lines(
    document: Option<&RecapDocument>,
    chapter: usize,
    p: &Palette,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let Some(document) = document else {
        return lines;
    };
    let mut push = |text: String, style: Style| {
        lines.push(Line::from_spans(vec![Span::styled(text, style)]));
    };
    if let Some(section) = document.sections.get(chapter) {
        push(
            format!("{:02}  /  {}", chapter + 1, document.sections.len()),
            Style::new().fg(p.violet),
        );
        push(section.title.clone(), p.title());
        push(String::new(), p.text());
        for block in section.blocks.iter().take(128) {
            match &block.content {
                BlockContent::Text { text } => push(text.clone(), p.text()),
                BlockContent::Image { asset_id, caption } => {
                    push("▧  SOURCE STILL".into(), Style::new().fg(p.accent));
                    push(caption.clone(), p.text());
                    if let Some(asset) = document.assets.iter().find(|asset| asset.id == *asset_id)
                    {
                        push(
                            format!(
                                "{}–{} · {}",
                                timestamp(asset.range.start_ms),
                                timestamp(asset.range.end_ms),
                                asset.path.as_str()
                            ),
                            p.quiet(),
                        );
                    }
                    push("Image available in exported HTML · terminal preview uses detected image capabilities".into(),p.quiet());
                }
                BlockContent::Clip { caption, asset_id } => {
                    push(format!("▸  {caption}"), p.text());
                    if let Some(asset) = document.assets.iter().find(|asset| asset.id == *asset_id)
                    {
                        push(
                            format!(
                                "Source {}–{} · play in HTML",
                                timestamp(asset.range.start_ms),
                                timestamp(asset.range.end_ms)
                            ),
                            p.quiet(),
                        );
                    }
                }
                BlockContent::Comparison { items } => {
                    push("SOURCE COMPARISON".into(), p.title());
                    for item in items {
                        push(format!("▧ {} · {}", item.label, item.asset_id), p.text());
                    }
                }
            }
            for id in block.evidence_ids.iter().take(8) {
                if let Some(evidence) = document.evidence.iter().find(|e| e.id == *id) {
                    push(
                        format!(
                            "↗ Source {}–{}",
                            timestamp(evidence.range.start_ms),
                            timestamp(evidence.range.end_ms)
                        ),
                        Style::new().fg(p.accent),
                    );
                }
            }
            push(String::new(), p.text());
        }
    }
    lines
}
fn timestamp(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}.{:03}", s / 60, s % 60, ms % 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responsive_modes_have_explicit_degradation() {
        assert_eq!(density(0, 0), Density::Tiny);
        assert_eq!(density(33, 40), Density::Tiny);
        assert_eq!(density(60, 30), Density::Narrow);
        assert_eq!(density(90, 30), Density::Medium);
        assert_eq!(density(140, 40), Density::Wide);
    }
    #[test]
    fn source_times_are_not_percentages() {
        assert_eq!(timestamp(125_999), "02:05.999");
        assert_eq!(timestamp(0), "00:00.000");
    }
    #[test]
    fn palette_cycles_without_changing_configuration() {
        let p = Palette::dark();
        assert_eq!(p.next().next().next().base, p.base);
    }
}
