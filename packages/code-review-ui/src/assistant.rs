//! The app-wide review assistant: one docked conversation with a Bcode session, available on
//! the reviews home, in the new-review wizard and in every review.
//!
//! * [`panel`] is the panel the app owns: placement beside or under the screen, the
//!   conversation, the permission prompt, renaming and deleting, and the attachment of the
//!   session to the review on screen.
//! * [`button`] is the Ask AI header button.
//! * [`history`] merges the review app's sessions and the review store's bindings into the
//!   conversation history, tagged with where each conversation started; [`picker`] is the
//!   history picker over it.
//! * [`Place`] is what the app tells the panel about the screen: which screen, and on a review
//!   its id, title, stack layer, cursor place and the binding that attaches a session to it.

pub mod actions;
pub mod button;
pub mod chips;
pub mod context;
pub mod history;
pub mod models;
pub mod panel;
pub mod picker;
pub mod profile;

pub use panel::{Assistant, Dock, Outcome, Split};

use bcode_code_review_models::{ReviewAssistantBinding, ReviewAssistantTag};

/// The panel's actions a screen's command palette lists (and runs on the panel).
pub const PALETTE_ACTIONS: &[&str] = &[
    "assistant_guide_tab",
    "assistant_model",
    "assistant_effort",
    "assistant_permissions",
    "assistant_tools",
    "assistant_new",
    "assistant_history",
    "assistant_open",
    "assistant_delete",
    "assistant_delete_all",
    "assistant_reset_size",
];

/// The command palette entries of the panel's actions, with their keys in the panel.
#[must_use]
pub fn palette_entries(host: &crate::context::UiContext) -> Vec<crate::palette::OverlayEntry> {
    let actions: Vec<bcode_keymap::KeymapAction> = crate::keymap::actions_for(ASSISTANT_SCOPE)
        .into_iter()
        .filter(|action| {
            PALETTE_ACTIONS
                .iter()
                .any(|name| action.id == format!("bcode.code_review.{name}"))
        })
        .map(|mut action| {
            action.description = format!("Assistant: {}", action.description);
            action
        })
        .collect();
    crate::palette::entries(&actions, |id| host.keymap_keys(ASSISTANT_SCOPE, id), None)
}

/// Keymap scope of the focused panel.
pub const ASSISTANT_SCOPE: &str = crate::keymap::ASSISTANT_SCOPE;
/// Keymap scope of the history picker.
pub const HISTORY_SCOPE: &str = crate::keymap::ASSISTANT_HISTORY_SCOPE;

/// What a screen keeps for the app's assistant.
///
/// That is whether the user asked it to open, focus or close the panel (a key or the palette),
/// and the cells left free at the end of its header row for the Ask AI button.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScreenLink {
    /// The user asked to open, focus or close the panel.
    pub requested: bool,
    /// Cells left free at the end of the header row.
    pub header_reserve: u16,
    /// What the screen asked of the assistant since the last call, oldest first.
    pub requests: Vec<LinkRequest>,
}

/// Something a screen asks of the assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRequest {
    /// Run one of the panel's actions (from a screen's palette).
    Action(String),
    /// Open the model picker for a target.
    Model(models::Target),
    /// Cycle a target's reasoning effort.
    Effort(models::Target),
    /// Send `text` as the user's message in the panel's conversation (`send`), or put it in
    /// the message input for the user to finish.
    Message {
        /// The message.
        text: String,
        /// Whether it is sent at once.
        send: bool,
    },
    /// Send `text` at once for a guide action, with the session bound with `grant` for it and
    /// `context` carried with it.
    GuideMessage {
        /// The message.
        text: String,
        /// What the session may change in the guide for this message.
        grant: Option<bcode_code_review_models::GuideGrant>,
        /// Context blocks (the section and its citations).
        context: Vec<crate::host::ContextBlock>,
        /// Reported back to the screen once the message's turn ended (see
        /// [`next_guide_ticket`]).
        ticket: Option<u64>,
    },
    /// Open the guide: its panel where the screen has room for it, else the assistant's Guide
    /// tab.
    OpenGuide,
}

/// A new ticket for a [`LinkRequest::GuideMessage`], unique for the life of the app.
#[must_use]
pub fn next_guide_ticket() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl ScreenLink {
    /// Whether the panel was asked for since the last call.
    pub const fn take_request(&mut self) -> bool {
        let requested = self.requested;
        self.requested = false;
        requested
    }

    /// What the screen asked of the assistant since the last call.
    pub fn take_requests(&mut self) -> Vec<LinkRequest> {
        std::mem::take(&mut self.requests)
    }
}

/// The screen the user is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The reviews home.
    Home,
    /// The new-review wizard.
    Wizard,
    /// An entry resolving before its review opens.
    Opening,
    /// A review.
    Review,
}

impl Screen {
    /// Stable name (`home`, `wizard`, `opening`, `review`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::Wizard => "wizard",
            Self::Opening => "opening",
            Self::Review => "review",
        }
    }
}

/// The review on screen, as the panel needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPlace {
    /// Annotation scope id of the review.
    pub workspace_id: String,
    /// Its title.
    pub title: String,
    /// Number of the current stack layer, when any.
    pub layer: Option<u64>,
    /// The cursor's place (`path:line`, a range, or a file), when in a file.
    pub chip: Option<String>,
    /// Changes whenever the review reopens (its binding must be refreshed).
    pub generation: u64,
    /// The binding that attaches a session to this review (its session id empty); left out
    /// where only the place's identity is needed.
    pub binding: Option<Box<ReviewAssistantBinding>>,
}

/// What is on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    /// The screen.
    pub screen: Screen,
    /// The review, on a review screen.
    pub review: Option<ReviewPlace>,
    /// What the user is looking at, in full; taken only when a message is sent or the context
    /// tool is called.
    pub context: Option<Box<context::ViewContext>>,
}

/// Identity of a place: changes when the session's attachment must change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaceKey {
    screen: Screen,
    workspace_id: Option<String>,
    generation: u64,
}

impl Place {
    /// A screen without a review.
    #[must_use]
    pub const fn screen(screen: Screen) -> Self {
        Self {
            screen,
            review: None,
            context: None,
        }
    }

    /// Identity of the place for attachment.
    #[must_use]
    pub fn key(&self) -> PlaceKey {
        PlaceKey {
            screen: self.screen,
            workspace_id: self
                .review
                .as_ref()
                .map(|review| review.workspace_id.clone()),
            generation: self.review.as_ref().map_or(0, |review| review.generation),
        }
    }

    /// The context chip: the cursor's place on a review, else the screen.
    #[must_use]
    pub fn chip(&self) -> String {
        match (&self.review, self.screen) {
            (Some(review), _) => review.chip.clone().unwrap_or_else(|| review.title.clone()),
            (None, Screen::Home) => "home".to_owned(),
            (None, Screen::Wizard) => "new review".to_owned(),
            (None, Screen::Opening | Screen::Review) => "opening a review".to_owned(),
        }
    }

    /// Where a conversation started here is tagged.
    #[must_use]
    pub fn tag(&self) -> ReviewAssistantTag {
        self.review
            .as_ref()
            .map_or_else(ReviewAssistantTag::none, |review| ReviewAssistantTag {
                workspace_id: Some(review.workspace_id.clone()),
                title: Some(review.title.clone()),
                layer: review.layer,
            })
    }

    /// The tag as the history shows it.
    #[must_use]
    pub fn history_tag(&self) -> history::Tag {
        self.review
            .as_ref()
            .map_or(history::Tag::Home, |review| history::Tag::Review {
                workspace_id: review.workspace_id.clone(),
                title: review.title.clone(),
                layer: review.layer,
            })
    }
}
