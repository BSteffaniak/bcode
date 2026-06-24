//! Central registry of documentation pages.

use hyperchad_docs_site::{DocPage, DocsSection, docs_generated_page, docs_markdown_page};

use crate::pages::docs;

const GETTING_STARTED: &str = "getting-started";
const REFERENCE: &str = "reference";
const CAPABILITIES: &str = "capabilities";
const OPERATIONS: &str = "operations";
const DEVELOPMENT: &str = "development";

/// Sidebar sections, in display order.
pub static DOC_SECTIONS: &[DocsSection] = &[
    DocsSection::new(GETTING_STARTED, "Getting Started"),
    DocsSection::new(REFERENCE, "Reference"),
    DocsSection::new(CAPABILITIES, "Capabilities"),
    DocsSection::new(OPERATIONS, "Operations"),
    DocsSection::new(DEVELOPMENT, "Development"),
];

fn generate_overview() -> String {
    include_str!("../../../../README.md").to_string()
}

fn generate_tui_keybindings() -> String {
    let readme = include_str!("../../../../README.md");
    docs::extract_section_for(readme, "## TUI keybindings", Some("## "))
}

fn generate_cli() -> String {
    docs::generate_cli_reference()
}

fn generate_config() -> String {
    docs::generate_config_reference()
}

/// Every doc page served by the site, in sidebar display order within each
/// section.
pub static DOC_PAGES: &[DocPage] = &[
    docs_generated_page! {
        route: "/docs",
        title: None,
        section: GETTING_STARTED,
        nav_label: "Overview",
        generate: generate_overview,
    },
    docs_generated_page! {
        route: "/docs/tui-keybindings",
        title: "TUI Keybindings",
        section: GETTING_STARTED,
        nav_label: "TUI Keybindings",
        generate: generate_tui_keybindings,
    },
    docs_generated_page! {
        route: "/docs/cli",
        title: "CLI Reference",
        section: REFERENCE,
        nav_label: "CLI",
        generate: generate_cli,
    },
    docs_generated_page! {
        route: "/docs/config",
        title: "Configuration Reference",
        section: REFERENCE,
        nav_label: "Config",
        generate: generate_config,
    },
    docs_markdown_page! {
        source: "docs/skills.md",
        route: "/docs/skills",
        title: "Skills",
        section: CAPABILITIES,
        nav_label: "Skills",
    },
    docs_markdown_page! {
        source: "docs/worktrees.md",
        route: "/docs/worktrees",
        title: "Worktrees",
        section: CAPABILITIES,
        nav_label: "Worktrees",
    },
    docs_markdown_page! {
        source: "docs/permissions.md",
        route: "/docs/permissions",
        title: "Permissions",
        section: CAPABILITIES,
        nav_label: "Permissions",
    },
    docs_markdown_page! {
        source: "docs/session-import-plugins.md",
        route: "/docs/session-import-plugins",
        title: "Session Import Plugins",
        section: CAPABILITIES,
        nav_label: "Session Imports",
    },
    docs_markdown_page! {
        source: "docs/openai-subscription-failover.md",
        route: "/docs/openai-subscription-failover",
        title: "OpenAI Subscription Failover",
        section: OPERATIONS,
        nav_label: "OpenAI Failover",
    },
    docs_markdown_page! {
        source: "docs/release-builds.md",
        route: "/docs/release-builds",
        title: "Release Builds",
        section: OPERATIONS,
        nav_label: "Release Builds",
    },
    docs_markdown_page! {
        source: "docs/session-persistence-architecture.md",
        route: "/docs/session-persistence-architecture",
        title: "Session Persistence Architecture",
        section: DEVELOPMENT,
        nav_label: "Session Persistence",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn routes_are_unique() {
        let mut seen = HashSet::new();
        for page in DOC_PAGES {
            assert!(seen.insert(page.route), "duplicate route: {}", page.route);
        }
    }

    #[test]
    fn nav_pages_have_labels() {
        for page in DOC_PAGES {
            assert!(
                page.nav_label.is_some(),
                "missing nav label: {}",
                page.route
            );
        }
    }
}
