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
    pub(super) const fn text(self) -> Style {
        Style::new().fg(self.ink)
    }
    pub(super) const fn quiet(self) -> Style {
        Style::new().fg(self.muted)
    }
    pub(super) const fn title(self) -> Style {
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
    pub(super) fn label(text: impl Into<String>, style: Style) -> TextBlock {
        TextBlock::new(text.into()).style(style)
    }
    pub(super) fn button<'a>(
        &'a self,
        id: &str,
        label: &'a str,
        index: usize,
    ) -> ButtonComponent<'a, 'a> {
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
    pub(super) fn field<'a>(
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
                            self.document
                                .as_ref()
                                .and_then(|document| document.sections.get(self.chapter))
                                .map_or_else(
                                    || "A little more room, a better perspective.".into(),
                                    |section| {
                                        format!("Chapter {} · {}", self.chapter + 1, section.title)
                                    },
                                ),
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
        let mut header = Row::new().gap(2).child(Self::label(
            "◈  R E C A P",
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ));
        if mode != Density::Narrow {
            header = header.flex_child(1, Self::label("  A clearer view.", p.quiet()));
        }
        let header = header
            .child(self.button("home", "Home", 0))
            .child(self.button("settings-nav", "Settings", 66))
            .child(self.button("theme", "◐", 1));
        let body = if self.active.is_some() && self.live_detail.expanded {
            self.activity_view()
        } else if self.active.is_some() {
            self.working()
        } else if self.settings_state.open {
            self.settings_view()
        } else {
            match self.screen {
                Screen::Home => self.home(mode),
                Screen::Reader => self.reader(mode),
                Screen::Diagnostics => Element::new(
                    TextViewComponent::new("diagnostics", &self.failure_lines, &self.scroll)
                        .styles(TextViewStyles {
                            empty: p.quiet(),
                            text: p.text(),
                            background: Style::new().bg(p.panel),
                            scrollbar: ScrollbarStyles::default(),
                        }),
                ),
            }
        };
        let footer = Column::new()
            .child(Self::label(self.status.clone(), p.quiet()))
            .child(Self::label(
                if mode == Density::Narrow {
                    "F7 Settings · F9 Details · F10/Ctrl+C Quit"
                } else {
                    "Enter Create   F7 Settings   F2 Generate   F3 Chapters   F4 Focus   F5 Export   F6 Open   F9 Details   F10/Ctrl+C Quit"
                },
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
        if let Some(approval) = &self.duration_approval {
            return Element::new(
                Column::new()
                    .child(Self::label("Longer video · your choice", p.title()))
                    .child(Self::label(approval.message(), p.text()))
                    .child(self.button("approve-duration", "Run anyway for this video", 68))
                    .child(self.button("decline-duration", "Cancel · keep defaults", 69)),
            );
        }
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
        }
        form = form.child(Self::label(self.connection_summary(), p.quiet()));
        form = form.child(
            Row::new()
                .gap(1)
                .child(self.button("generate", "Generate  ↗", 2))
                .child(self.button(
                    "options",
                    if self.refine {
                        "Hide source options"
                    } else {
                        "Source options"
                    },
                    8,
                ))
                .child(self.button("settings", "Settings", 67)),
        );
        form = form.child(self.button("resume", "Resume saved generation", 70));
        if let Some(id) = &self.resume_id {
            form = form.child(Self::label(format!("Saved generation · {id}"), p.quiet()));
            form = form.child(Self::label("Unresolved calls require explicit retry approval; they may have incurred AI usage.",p.text()))
                .child(self.button("retry-uncertain", "Approve retry of unresolved calls", 71));
        }
        form = form.child(self.button(
            "depth",
            match self.depth {
                bcode_recap::contracts::Depth::Brief => "Depth · Brief",
                bcode_recap::contracts::Depth::Standard => "Depth · Standard",
                bcode_recap::contracts::Depth::Detailed => "Depth · Detailed — readable video",
            },
            3,
        ));
        if self.depth == bcode_recap::contracts::Depth::Detailed {
            form = form.child(Self::label(
                "Detailed inventories the source, writes chapters, audits coverage and repairs gaps. More model work; limitations remain explicit.",
                p.quiet(),
            ));
        }
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
    fn activity_view(&self) -> Element<'_> {
        let p = self.palette;
        Element::new(
            Column::new()
                .child(Self::label(
                    "RECENT ACTIVITY · provisional output, not the accepted edition",
                    p.title(),
                ))
                .child(self.button("live-detail", "← Back to progress · F3", 77))
                .flex(Flex::new(
                    1,
                    TextViewComponent::new(
                        "activity-history",
                        &self.live_detail.lines,
                        &self.live_detail.scroll,
                    )
                    .styles(TextViewStyles {
                        text: p.text(),
                        background: Style::new().bg(p.panel),
                        ..TextViewStyles::default()
                    }),
                )),
        )
    }
    fn working(&self) -> Element<'_> {
        let p = self.palette;
        let active = self
            .active
            .as_ref()
            .expect("working screen has an operation");
        let (stage, counts) = match active.stage {
            Some(Stage::AnalysisProgress {
                phase,
                completed,
                total,
            }) => (
                Some(Stage::from(phase)),
                Some(total.map_or_else(
                    || format!("{completed} work units completed · total not yet known"),
                    |total| format!("{completed} / {total} work units completed"),
                )),
            ),
            stage => (stage, None),
        };
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
        for label in self.activity_labels() {
            body = body.child(Self::label(label, p.quiet()));
        }
        body = body.child(self.button(
            "live-detail",
            if self.live_detail.expanded {
                "Hide live output · F3"
            } else {
                "Show live output · F3"
            },
            77,
        ));
        // Only the active stage is asserted: inactive stages may be skipped, repeated,
        // or revisited during repair. Keep cancellation visible on short terminals.
        for (current, label) in Self::working_steps() {
            if stage == Some(current) {
                body = body.child(Self::label(
                    format!("◉  {label}"),
                    Style::new().fg(p.accent),
                ));
            }
        }
        if let Some(counts) = counts {
            body = body.child(Self::label(counts, p.quiet()));
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
    const fn working_steps() -> [(Stage, &'static str); 9] {
        [
            (Stage::ModelReadiness, "00  Model readiness"),
            (Stage::Acquisition, "01  Source & captions"),
            (Stage::Media, "02  Source visuals"),
            (Stage::Composition, "03  Compose perspective"),
            (
                Stage::Analysis,
                "Analysis · source inventory & chapter plan",
            ),
            (Stage::Writing, "Writing · source-grounded chapters"),
            (Stage::Audit, "Audit · source coverage"),
            (Stage::Repair, "Repair · identified coverage gaps"),
            (Stage::Storage, "Validate & save"),
        ]
    }
    fn activity_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        for previous in self
            .stage_history
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            labels.push(format!("· {previous}"));
        }
        for (millis, event) in self.activity.recent(4) {
            let label = match event {
                bcode_recap::activity::Activity::Started { id, label } => {
                    format!("{millis}ms · #{id} {label}")
                }
                bcode_recap::activity::Activity::Finished { id, succeeded } => format!(
                    "#{id} {}",
                    if succeeded {
                        "completed"
                    } else {
                        "interrupted or failed"
                    }
                ),
                bcode_recap::activity::Activity::Reused => "✓ Reused completed work".into(),
                bcode_recap::activity::Activity::Text { id, text } => {
                    if self.live_detail.expanded {
                        format!("#{id} · Provisional output (not yet validated)\n{text}")
                    } else {
                        format!("#{id} · Receiving provisional output")
                    }
                }
            };
            labels.push(label);
        }
        labels
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
        let reading = super::reader::RetainedReader {
            view: reading,
            revision: self.preview_generation,
            layout: &self.reader_layout,
        };
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
    fn chapter_contents<'a>(&'a self, document: &'a RecapDocument) -> Column<'a> {
        let p = self.palette;
        let mut chapters = Column::new()
            .gap(1)
            .child(
                Row::new()
                    .child(Self::label("CONTENTS ▶", Style::new().fg(p.violet)))
                    .flex_child(
                        1,
                        self.button(
                            &format!("chapter-{}", self.chapter),
                            document
                                .sections
                                .get(self.chapter)
                                .map_or("No chapters", |section| section.title.as_str()),
                            48,
                        ),
                    ),
            )
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
        let (chapter_start, chapter_end) = self.chapter_window();
        for (index, section) in document
            .sections
            .iter()
            .enumerate()
            .skip(chapter_start)
            .take(chapter_end.saturating_sub(chapter_start))
        {
            if index == self.chapter {
                continue;
            }
            chapters = chapters.child(
                Row::new()
                    .child(Self::label(
                        if index == self.chapter { "▶" } else { "·" },
                        p.quiet(),
                    ))
                    .flex_child(
                        1,
                        self.button(
                            &format!("chapter-{index}"),
                            &section.title,
                            index - chapter_start + 40,
                        ),
                    ),
            );
        }
        if chapter_start > 0 {
            chapters = chapters.child(self.button("chapter-prev", "← Earlier chapters", 77));
        }
        if chapter_end < document.sections.len() {
            chapters = chapters.child(self.button("chapter-next", "Later chapters →", 78));
        }
        chapters
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
        let chapters = self.chapter_contents(document);
        // On collapsed layouts, contents replaces the reader form entirely.
        // Export/refinement fields must not consume the drawer's flex allocation.
        if mode != Density::Wide && self.drawer {
            return Element::new(
                Surface::new(
                    Column::new()
                        .child(self.button("chapters", "Close contents · F3", 7))
                        .flex(Flex::new(1, chapters)),
                )
                .background(Style::new().bg(p.panel))
                .content_style(p.text().bg(p.panel))
                .padding(LogicalInsets::all(1)),
            );
        }
        let middle = if mode == Density::Wide {
            Element::new(
                Row::new()
                    .gap(2)
                    .flex_child(1, Surface::new(chapters).padding(LogicalInsets::all(1)))
                    .flex_child(3, content),
            )
        } else {
            Element::new(content)
        };
        let mut body = Column::new()
            .gap(u64::from(mode == Density::Wide))
            .child(Self::label(document.title.clone(), p.title()))
            .child(actions)
            .child(Self::label(
                format!(
                    "Chapter {} / {} · F11 previous · F12 next",
                    self.chapter + 1,
                    document.sections.len()
                ),
                p.quiet(),
            ))
            .child(
                Row::new()
                    .flex_child(1, self.button("previous-section", "← Previous", 75))
                    .flex_child(1, self.button("next-section", "Next →", 76))
                    .flex_child(1, self.button("last-section", "Last chapter", 79)),
            )
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

fn is_illustration(document: &RecapDocument, id: &str) -> bool {
    document
        .assets
        .iter()
        .find(|asset| asset.id == id)
        .is_some_and(|asset| asset.illustration.is_some())
}
fn semantic_lines(content: &BlockContent, p: &Palette, push: &mut impl FnMut(String, Style)) {
    match content {
        BlockContent::List { ordered, items } => {
            for (index, item) in items.iter().enumerate() {
                push(
                    if *ordered {
                        format!("{}. {item}", index + 1)
                    } else {
                        format!("• {item}")
                    },
                    p.text(),
                );
            }
        }
        BlockContent::Diagram { caption, steps } => {
            push(caption.clone(), p.title());
            for (index, item) in steps.iter().enumerate() {
                push(format!("{} → {item}", index + 1), p.text());
            }
        }
        _ => {}
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
        push("Source times refer to the original video; playback is available in HTML. Reported coverage limitations remain in chapter text, including the final chapter.".into(), p.quiet());
        push(String::new(), p.text());
        for block in &section.blocks {
            match &block.content {
                BlockContent::Heading { text } => push(text.clone(), p.title()),
                BlockContent::List { .. } | BlockContent::Diagram { .. } => {
                    semantic_lines(&block.content, p, &mut push);
                }
                BlockContent::Text { text } => push(text.clone(), p.text()),
                BlockContent::Image { asset_id, caption } => {
                    let external = is_illustration(document, asset_id);
                    push(
                        if external {
                            "▧ SUPPORTING ILLUSTRATION"
                        } else {
                            "▧ SOURCE STILL"
                        }
                        .into(),
                        Style::new().fg(p.accent),
                    );
                    push(caption.clone(), p.text());
                    if let Some(asset) = document.assets.iter().find(|asset| asset.id == *asset_id)
                    {
                        if external {
                            push(
                                "Supporting illustration · attribution in HTML".into(),
                                p.quiet(),
                            );
                        } else {
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
            for id in &block.evidence_ids {
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
    if s >= 3600 {
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            s / 3600,
            (s / 60) % 60,
            s % 60,
            ms % 1000
        )
    } else {
        format!("{:02}:{:02}.{:03}", s / 60, s % 60, ms % 1000)
    }
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
    fn reader_retains_every_block_and_source_reference() {
        use bcode_recap::contracts::{
            Block, Evidence, GenerationIntent, Section, Source, TimeRange,
        };
        let document = RecapDocument {
            schema_version: 1,
            id: "long-reader".into(),
            revision: 1,
            title: "Long source".into(),
            source: Source {
                context: bcode_recap::source_context::SourceContext::default(),
                id: "source".into(),
                title: "Source".into(),
                url: None,
                duration_ms: 7_200_000,
            },
            intent: GenerationIntent::default(),
            transcript: vec![],
            assets: vec![],
            evidence: (0..12)
                .map(|index| Evidence {
                    id: format!("e-{index}"),
                    range: TimeRange {
                        start_ms: index * 1000,
                        end_ms: index * 1000 + 999,
                    },
                    transcript_segment_ids: vec![],
                })
                .collect(),
            sections: vec![Section {
                id: "chapter".into(),
                title: "日本語 👩‍💻 e\u{301}".into(),
                blocks: (0..160)
                    .map(|index| Block {
                        id: format!("b-{index}"),
                        evidence_ids: (0..12).map(|id| format!("e-{id}")).collect(),
                        content: BlockContent::Text {
                            text: format!("Complete paragraph {index}"),
                        },
                    })
                    .collect(),
            }],
        };
        let lines = reader_lines(Some(&document), 0, &Palette::dark());
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_str()))
            .collect();
        assert!(text.contains("Complete paragraph 159"));
        assert!(text.contains("00:11.000"));
        assert!(text.contains("日本語 👩‍💻 e\u{301}"));
    }
    #[test]
    fn source_times_are_not_percentages() {
        assert_eq!(timestamp(3_725_999), "01:02:05.999");
        assert_eq!(timestamp(125_999), "02:05.999");
        assert_eq!(timestamp(0), "00:00.000");
    }
    #[test]
    fn palette_cycles_without_changing_configuration() {
        let p = Palette::dark();
        assert_eq!(p.next().next().next().base, p.base);
    }
}
