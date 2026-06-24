//! Home page — landing page with no sidebar.

use hyperchad::actions::logic::if_responsive;
use hyperchad::template::{Containers, container};
use hyperchad::transformer::models::LayoutDirection;

use crate::home_layout as layout;

/// The landing page for bcode docs site.
#[must_use]
pub fn home() -> Containers {
    container! {
        div
            #hero-wrap
            flex-grow=1
            justify-content=center
            align-items=center
            padding-x=(if_responsive("tablet").then::<i32>(24).or_else(80))
            padding-y=(if_responsive("mobile").then::<i32>(48).or_else(80))
        {
            (hero())
            (features())
        }
    }
}

fn hero() -> Containers {
    container! {
        div
            #hero-inner
            align-items=center
            max-width=820
            gap=(if_responsive("mobile").then::<i32>(24).or_else(32))
        {
            div
                #hero-brand
                color=(layout::green())
                font-size=(if_responsive("tablet").then::<i32>(40).or_else(64))
                font-family=(layout::MONO_FONT)
                text-align=center
            {
                ">_ bcode"
            }

            h1
                #hero-tagline
                color=(layout::text_primary())
                font-size=(if_responsive("tablet").then::<i32>(22).or_else(32))
                font-family=(layout::MONO_FONT)
                text-align=center
                margin-bottom=16
            {
                "Terminal-native coding agent"
            }

            div
                #hero-description
                color=(layout::text_secondary())
                font-size=(if_responsive("mobile").then::<i32>(14).or_else(16))
                font-family=(layout::MONO_FONT)
                text-align=center
                max-width=680
            {
                "Bcode is a Rust-native, TUI-first coding agent with local client/server architecture, plugin-driven tools, skills, and model providers."
            }

            (hero_actions())
        }
    }
}

fn hero_actions() -> Containers {
    container! {
        div
            #hero-actions
            direction=row
            gap=16
            margin-top=32
            justify-content=center
        {
            anchor
                href="/docs"
                background=(layout::green())
                color=#0d1117
                padding-x=24
                padding-y=12
                border-radius=6
                text-decoration="none"
                font-family=(layout::MONO_FONT)
                font-size=14
            {
                "read docs"
            }
            anchor
                href="https://github.com/BSteffaniak/bcode"
                target="_blank"
                border="1, #7ee787"
                color=(layout::green())
                padding-x=24
                padding-y=12
                border-radius=6
                text-decoration="none"
                font-family=(layout::MONO_FONT)
                font-size=14
            {
                "view on github"
            }
        }
    }
}

fn features() -> Containers {
    container! {
        div
            #features-row
            direction=(
                if_responsive("tablet")
                    .then::<LayoutDirection>(LayoutDirection::Column)
                    .or_else(LayoutDirection::Row)
            )
            gap=24
            margin-top=(if_responsive("mobile").then::<i32>(48).or_else(80))
            max-width=960
        {
            (feature_card(
                0,
                "terminal-native",
                "Designed around a keyboard-first TUI workflow instead of a web chat surface."
            ))
            (feature_card(
                1,
                "plugin-driven",
                "Tools, providers, skills, and integrations are modeled as first-class plugins."
            ))
            (feature_card(
                2,
                "configurable",
                "CLI and config references are generated from the same Rust code used by the app."
            ))
        }
    }
}

fn feature_card(index: u8, title: &str, description: &str) -> Containers {
    let id = format!("feature-card-{index}");
    container! {
        div
            id=(id)
            background=(layout::surface())
            border-radius=8
            padding=(if_responsive("mobile").then::<i32>(20).or_else(24))
            flex=1
            border-left="2, #7ee787"
        {
            h3
                color=(layout::text_primary())
                font-size=15
                font-family=(layout::MONO_FONT)
                margin-bottom=8
            {
                (title)
            }
            div
                color=(layout::text_muted())
                font-size=13
                font-family=(layout::MONO_FONT)
            {
                (description)
            }
        }
    }
}
