//! What the model switcher can offer, independent of how it is drawn.
//!
//! Discovery reports one [`SourceUpdate`] per source (a native tool, a provider, an approved
//! credential connection, or credentials that could be connected). The inventory keeps the
//! latest report of each and derives the sections the switcher shows: the current choice,
//! favorites, recent choices, then each source in a fixed order.

use bcode_command::completion::{CompletionMatchMode, completion_match_rank};
use bcode_model::execution::{
    ExecutionBillingRoute, ExecutionModel, ExecutionModelAccess, ExecutionModelReadiness,
    ExecutionRoute,
};
use bcode_model::{ModelCapability, ModelInfo, ModelTokenPrice};
use bcode_provider_auth_models::external::ExternalAuthCandidate;
use std::collections::{BTreeMap, BTreeSet};

/// Where a row's model runs, ordered as the switcher lists sources.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceKey {
    /// A native coding tool Bcode runs, such as Claude Code.
    Native { instance_id: String },
    /// Another agent's credentials, approved for use in Bcode.
    Connection { binding_id: String },
    /// A Bcode model provider using its own configured access.
    Provider { plugin_id: String },
    /// Credentials found on this machine that could be connected.
    Connect,
}

/// Progress of one source's discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceState {
    /// Still being checked.
    Loading,
    /// Reported its models.
    Ready,
    /// Could not be checked; the text says why in user terms.
    Failed(String),
}

/// A source heading: what it is and how it is billed or reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Identity and order.
    pub key: SourceKey,
    /// Name, such as "Claude Code" or "`OpenAI`".
    pub title: String,
    /// Qualifier, such as "subscription" or "via Codex".
    pub detail: String,
    /// Discovery progress.
    pub state: SourceState,
}

/// How a row reaches its model, beyond its route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// The route's own configured access.
    Direct,
    /// An approved connection to another agent's credentials.
    Connection {
        /// Connection binding, sent with the selection.
        binding_id: String,
        /// Application whose credentials are used, such as "Codex".
        application: String,
        /// Where the credentials are read from, for approving them again.
        target: bcode_provider_auth_models::external::ExternalAuthTarget,
    },
    /// Credentials that need approval before their models can be used.
    Connect(Box<ExternalAuthCandidate>),
    /// An installed native tool that needs setup or sign-in before it can run here.
    Setup {
        /// Tool name, such as "Codex".
        tool: String,
    },
}

/// Something the user can do to make an unusable row usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fix {
    /// Approve discovered credentials.
    Connect,
    /// Approve a connection again after its account changed.
    Reapprove,
    /// Set up or sign in to a native tool.
    Setup,
}

/// Whether a row can be chosen now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Access to this exact model is confirmed.
    Confirmed,
    /// The route is ready; the model itself is checked on first use.
    Available,
    /// Usable after the user acts.
    Action {
        /// What to do.
        fix: Fix,
        /// Why, in user terms.
        reason: String,
    },
    /// Not usable now; the reason names the cause.
    Unavailable(String),
}

impl Readiness {
    /// Whether choosing the row selects its model directly.
    #[must_use]
    pub const fn selectable(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Available)
    }

    /// Readiness reported by a route's adapter.
    #[must_use]
    pub fn from_execution(readiness: ExecutionModelReadiness, native: bool) -> Self {
        match readiness {
            ExecutionModelReadiness::Ready
            | ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Confirmed,
            } => Self::Confirmed,
            ExecutionModelReadiness::Unknown if native => Self::Action {
                fix: Fix::Setup,
                reason: "Sign-in status is unknown; open Setup to check it".into(),
            },
            ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Unknown,
            }
            | ExecutionModelReadiness::Unknown => Self::Available,
            ExecutionModelReadiness::RouteReady {
                model_access: ExecutionModelAccess::Unavailable,
            } => Self::Unavailable("Not available on this account".into()),
            ExecutionModelReadiness::Unavailable if native => Self::Action {
                fix: Fix::Setup,
                reason: "Not signed in; open Setup to sign in".into(),
            },
            ExecutionModelReadiness::Unavailable => Self::Unavailable("Unavailable".into()),
        }
    }
}

/// One choosable model on one source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable, nonsecret identity of this exact choice, also stored for recents and favorites.
    pub key: String,
    /// Source that lists it.
    pub source: SourceKey,
    /// The model, its route, and the selector sent to the route.
    pub model: ExecutionModel,
    /// How it is reached.
    pub access: Access,
    /// Whether it can be chosen now.
    pub readiness: Readiness,
}

impl Entry {
    /// Build an entry, deriving its key from the model, route, and access.
    #[must_use]
    pub fn new(
        source: SourceKey,
        model: ExecutionModel,
        access: Access,
        readiness: Readiness,
    ) -> Self {
        Self {
            key: choice_key(&model.route, &model.model.model_id, &access),
            source,
            model,
            access,
            readiness,
        }
    }

    /// The name people know the model by.
    #[must_use]
    pub fn title(&self) -> &str {
        match &self.access {
            Access::Connect(candidate) => &candidate.application,
            Access::Setup { tool } => tool,
            _ if self.model.model.display_name.trim().is_empty() => &self.model.model.model_id,
            _ => &self.model.model.display_name,
        }
    }

    /// The identifier the route knows it by, when that differs from the title.
    #[must_use]
    pub fn identifier(&self) -> Option<&str> {
        if matches!(self.access, Access::Connect(_) | Access::Setup { .. }) {
            return None;
        }
        let id = self.model.model.model_id.as_str();
        (id != self.title()).then_some(id)
    }

    /// The single most useful fact for comparing rows: price, context size, or billing.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        match &self.access {
            Access::Connect(_) => return Some("Use its credentials in Bcode".into()),
            Access::Setup { .. } => return Some("Set up to run it here".into()),
            Access::Direct | Access::Connection { .. } => {}
        }
        let model = &self.model.model;
        if self.model.billing_route == ExecutionBillingRoute::NativeSubscription {
            return model
                .context_window
                .map(|tokens| format!("{} context", token_count(tokens)));
        }
        input_price(model)
            .map(|price| format!("{price} in"))
            .or_else(|| {
                model
                    .context_window
                    .map(|tokens| format!("{} context", token_count(tokens)))
            })
    }

    fn search_text(&self) -> String {
        let mut text = vec![self.model.model.model_id.clone()];
        text.extend(capability_labels(&self.model.model).map(str::to_owned));
        if let Access::Connection { application, .. } = &self.access {
            text.push(application.clone());
        }
        text.join(" ")
    }
}

/// Nonsecret identity of a choice. Connection bindings are digests, never credentials.
#[must_use]
pub fn choice_key(route: &ExecutionRoute, model_id: &str, access: &Access) -> String {
    let route = match route {
        ExecutionRoute::ModelProvider { provider_plugin_id } => {
            format!("provider:{provider_plugin_id}")
        }
        ExecutionRoute::ManagedHarness { instance_id, .. } => format!("native:{instance_id}"),
    };
    let access = match access {
        Access::Direct => String::new(),
        Access::Connection { binding_id, .. } => format!("connection:{binding_id}"),
        Access::Connect(candidate) => format!("connect:{}", candidate.binding_id),
        Access::Setup { tool } => format!("setup:{tool}"),
    };
    format!("{route}|{model_id}|{access}")
}

/// One source's latest discovery report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUpdate {
    /// The source, including its progress.
    pub source: Source,
    /// Everything it offers, in the order it listed them.
    pub entries: Vec<Entry>,
}

/// The model the conversation currently uses, as far as the host knows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentChoice {
    /// Route of the current model, when known.
    pub route: Option<ExecutionRoute>,
    /// Requested model identifier.
    pub model_id: Option<String>,
}

impl CurrentChoice {
    fn matches(&self, entry: &Entry) -> bool {
        self.model_id.as_deref() == Some(entry.model.model.model_id.as_str())
            && self
                .route
                .as_ref()
                .is_none_or(|route| match (route, &entry.model.route) {
                    (
                        ExecutionRoute::ModelProvider { provider_plugin_id },
                        ExecutionRoute::ModelProvider {
                            provider_plugin_id: other,
                        },
                    ) => provider_plugin_id == other,
                    (
                        ExecutionRoute::ManagedHarness { instance_id, .. },
                        ExecutionRoute::ManagedHarness {
                            instance_id: other, ..
                        },
                    ) => instance_id == other,
                    _ => false,
                })
            && !matches!(entry.access, Access::Connect(_) | Access::Setup { .. })
    }
}

/// A heading in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SectionKind {
    /// The model in use.
    Current,
    /// Marked by the user.
    Favorites,
    /// Chosen recently.
    Recent,
    /// Everything one source offers.
    Source(SourceKey),
}

/// A list row: a heading or a choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A section heading.
    Heading {
        /// Which section.
        kind: SectionKind,
        /// Heading text.
        title: String,
        /// Muted qualifier.
        detail: String,
        /// The source is still loading.
        loading: bool,
        /// The source failed; the text says why.
        failure: Option<String>,
    },
    /// A choice, by index into [`Inventory::entries`].
    Choice {
        /// Unique row identity within the list (a choice can appear in several sections).
        id: String,
        /// Entry index.
        entry: usize,
        /// Whether this is the model in use.
        current: bool,
        /// Whether it is a favorite.
        favorite: bool,
    },
}

/// Everything discovered so far.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    sources: BTreeMap<SourceKey, Source>,
    entries: Vec<Entry>,
}

impl Inventory {
    /// Replace one source's report.
    pub fn apply(&mut self, update: SourceUpdate) {
        let key = update.source.key.clone();
        self.entries.retain(|entry| entry.source != key);
        self.entries.extend(update.entries);
        self.sources.insert(key, update.source);
    }

    /// Mark a source as loading, keeping what it reported before.
    pub fn loading(&mut self, source: Source) {
        self.sources.insert(source.key.clone(), source);
    }

    /// All sources, in listing order.
    pub fn sources(&self) -> impl Iterator<Item = &Source> {
        self.sources.values()
    }

    /// All entries.
    #[must_use]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// One entry by key.
    #[must_use]
    pub fn entry(&self, key: &str) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// Whether any source is still loading.
    #[must_use]
    pub fn loading_any(&self) -> bool {
        self.sources
            .values()
            .any(|source| source.state == SourceState::Loading)
    }

    /// Rows to show. Without a query: current, favorites, recent, then sources. With a query:
    /// only matching choices, ranked within their sources, and no duplicate sections.
    #[must_use]
    pub fn rows(
        &self,
        query: &str,
        current: &CurrentChoice,
        favorites: &BTreeSet<String>,
        recent: &[String],
    ) -> Vec<Row> {
        let query = query.trim();
        let mut rows = Vec::new();
        let choice = |section: &str, index: usize| Row::Choice {
            id: format!("{section}/{}", self.entries[index].key),
            entry: index,
            current: current.matches(&self.entries[index]),
            favorite: favorites.contains(&self.entries[index].key),
        };
        if query.is_empty() {
            let current_rows: Vec<_> = (0..self.entries.len())
                .filter(|index| current.matches(&self.entries[*index]))
                .take(1)
                .collect();
            let favorite_rows: Vec<_> = (0..self.entries.len())
                .filter(|index| favorites.contains(&self.entries[*index].key))
                .collect();
            let recent_rows: Vec<_> = recent
                .iter()
                .filter_map(|key| self.entries.iter().position(|entry| entry.key == *key))
                .filter(|index| !current_rows.contains(index))
                .collect();
            for (kind, title, indexes, section) in [
                (SectionKind::Current, "Current", current_rows, "current"),
                (
                    SectionKind::Favorites,
                    "Favorites",
                    favorite_rows,
                    "favorites",
                ),
                (SectionKind::Recent, "Recent", recent_rows, "recent"),
            ] {
                if indexes.is_empty() {
                    continue;
                }
                rows.push(Row::Heading {
                    kind,
                    title: title.into(),
                    detail: String::new(),
                    loading: false,
                    failure: None,
                });
                rows.extend(indexes.into_iter().map(|index| choice(section, index)));
            }
        }
        for source in self.sources.values() {
            let mut indexes: Vec<_> = (0..self.entries.len())
                .filter(|index| self.entries[*index].source == source.key)
                .filter_map(|index| {
                    if query.is_empty() {
                        return Some((None, index));
                    }
                    let entry = &self.entries[index];
                    let name = format!("{} {}", entry.title(), source.title);
                    completion_match_rank(
                        &name,
                        &entry.search_text(),
                        query,
                        CompletionMatchMode::Fuzzy,
                    )
                    .map(|rank| (Some(rank), index))
                })
                .collect();
            if !query.is_empty() && indexes.is_empty() {
                continue;
            }
            indexes.sort_by_key(|(rank, _)| *rank);
            let failure = match &source.state {
                SourceState::Failed(reason) => Some(reason.clone()),
                _ => None,
            };
            rows.push(Row::Heading {
                kind: SectionKind::Source(source.key.clone()),
                title: source.title.clone(),
                detail: source.detail.clone(),
                loading: source.state == SourceState::Loading,
                failure,
            });
            rows.extend(
                indexes
                    .into_iter()
                    .map(|(_, index)| choice("source", index)),
            );
        }
        rows
    }
}

/// Short labels for what a model supports.
pub fn capability_labels(model: &ModelInfo) -> impl Iterator<Item = &'static str> + '_ {
    [
        (ModelCapability::ToolCalls, "tools"),
        (ModelCapability::ImageInput, "vision"),
        (ModelCapability::Reasoning, "reasoning"),
        (ModelCapability::PromptCaching, "caching"),
        (ModelCapability::NativeWebSearch, "web search"),
    ]
    .into_iter()
    .filter(|(capability, _)| model.capabilities.contains(capability))
    .map(|(_, label)| label)
}

/// A token count as people read it: `200k`, `1M`, `1.5M`.
#[must_use]
pub fn token_count(value: u32) -> String {
    if value >= 1_000_000 {
        let whole = value / 1_000_000;
        let tenth = (value % 1_000_000) / 100_000;
        if tenth == 0 {
            format!("{whole}M")
        } else {
            format!("{whole}.{tenth}M")
        }
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

/// A per-million-token price such as `$1.25/M`.
#[must_use]
pub fn token_price(currency: &str, price: ModelTokenPrice) -> String {
    let whole = price.micros / 1_000_000;
    let fraction = price.micros % 1_000_000;
    let mut amount = if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:06}")
    };
    if fraction != 0 {
        while amount.ends_with('0') {
            amount.pop();
        }
    }
    if currency == "USD" {
        format!("${amount}/M")
    } else {
        format!("{currency} {amount}/M")
    }
}

fn input_price(model: &ModelInfo) -> Option<String> {
    let pricing = model.pricing.as_ref()?;
    Some(token_price(&pricing.currency, pricing.input?))
}

/// Pricing lines for the detail pane: input, cached input, cache writes, output.
#[must_use]
pub fn price_lines(model: &ModelInfo) -> Vec<(&'static str, String)> {
    let Some(pricing) = model.pricing.as_ref() else {
        return Vec::new();
    };
    [
        ("Input", pricing.input),
        ("Cached input", pricing.cached_input),
        ("Cache write", pricing.cache_write_input),
        ("Output", pricing.output),
    ]
    .into_iter()
    .filter_map(|(label, price)| Some((label, token_price(&pricing.currency, price?))))
    .collect()
}

/// How a route is billed, in user terms.
#[must_use]
pub const fn billing_label(route: ExecutionBillingRoute) -> Option<&'static str> {
    match route {
        ExecutionBillingRoute::NativeSubscription => Some("Your subscription"),
        ExecutionBillingRoute::ProviderApi => Some("Your provider API account"),
        ExecutionBillingRoute::Api => Some("API usage"),
        ExecutionBillingRoute::Unknown => None,
    }
}
