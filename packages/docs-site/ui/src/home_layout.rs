//! Styling helpers for the bcode docs site.

use hyperchad::actions::ActionType;
use hyperchad::actions::logic::if_responsive;
use hyperchad::color::Color;
use hyperchad::template::{Container, Containers, container};
use hyperchad::transformer::models::LayoutDirection;
use hyperchad_docs_site::{NavSection, ShellContext};

/// Terminal green accent color (#7ee787 — can't use hex literal because `e` parses as exponent).
#[must_use]
pub fn green() -> Color {
    Color::from_hex("#7ee787")
}

/// Light text color.
#[must_use]
pub fn text_primary() -> Color {
    Color::from_hex("#f0f6fc")
}

/// Muted text color.
#[must_use]
pub fn text_secondary() -> Color {
    Color::from_hex("#c9d1d9")
}

/// Muted/dim text color.
#[must_use]
pub fn text_muted() -> Color {
    Color::from_hex("#8b949e")
}

/// Dark surface background.
#[must_use]
pub fn surface() -> Color {
    Color::from_hex("#161b22")
}

/// Border color.
#[must_use]
pub fn border() -> Color {
    Color::from_hex("#21262d")
}

/// Monospace font stack.
pub const MONO_FONT: &str = "'SF Mono', 'Cascadia Code', 'Fira Code', Menlo, Consolas, monospace";

const SIDEBAR_ID: &str = "docs-sidebar";
const BACKDROP_ID: &str = "docs-backdrop";

/// Top navigation bar shown on every page.
#[must_use]
pub fn nav_bar() -> Containers {
    container! {
        header
            direction=row
            align-items=center
            background=#0d1117
            border-bottom="1, #21262d"
            padding-y=12
        {
            div
                #nav-left
                direction=row
                align-items=center
                padding-x=(if_responsive("mobile").then::<i32>(16).or_else(24))
                gap=(if_responsive("mobile").then::<i32>(12).or_else(0))
            {
                div
                    #hamburger-btn
                    hidden=(if_responsive("mobile").then::<bool>(false).or_else(true))
                    fx-click=(ActionType::Multi(vec![
                        ActionType::display_by_id(SIDEBAR_ID),
                        ActionType::display_by_id(BACKDROP_ID),
                    ]))
                    cursor="pointer"
                    color=(text_secondary())
                    font-size=20
                    padding=4
                    user-select="none"
                {
                    "\u{2630}"
                }
                anchor
                    color=(green())
                    direction=row
                    align-items=center
                    text-decoration="none"
                    href="/"
                {
                    span font-size=18 font-family=(MONO_FONT) {
                        ">_ bcode"
                    }
                }
            }
            div
                #nav-right
                direction=row
                align-items=center
                justify-content=end
                flex=1
                padding-x=(if_responsive("mobile").then::<i32>(16).or_else(24))
                gap=(if_responsive("mobile").then::<i32>(16).or_else(24))
            {
                anchor
                    color=(text_secondary())
                    text-decoration="none"
                    font-family=(MONO_FONT)
                    font-size=14
                    href="/docs"
                {
                    "docs"
                }
                anchor
                    color=(text_secondary())
                    text-decoration="none"
                    font-family=(MONO_FONT)
                    font-size=14
                    href="https://github.com/BSteffaniak/bcode"
                    target="_blank"
                {
                    "github"
                }
            }
        }
    }
}

fn sidebar_section(title: &str, items: &[Container]) -> Containers {
    container! {
        div padding-x=16 margin-bottom=16 {
            div
                color=(text_muted())
                font-size=11
                font-family=(MONO_FONT)
                margin-bottom=8
                padding-x=8
            {
                (title.to_uppercase())
            }
            @for item in items {
                (item)
            }
        }
    }
}

fn sidebar_item(label: &str, href: &str, active: bool) -> Container {
    if active {
        container! {
            anchor
                color=(green())
                text-decoration="none"
                font-family=(MONO_FONT)
                font-size=13
                padding-y=4
                padding-x=8
                border-radius=4
                background=(surface())
                href=(href)
            {
                (label)
            }
        }
    } else {
        container! {
            anchor
                color=(text_secondary())
                text-decoration="none"
                font-family=(MONO_FONT)
                font-size=13
                padding-y=4
                padding-x=8
                href=(href)
            {
                (label)
            }
        }
    }
    .into()
}

fn sidebar_sections(sections: &[NavSection], current_path: &str) -> Vec<Containers> {
    sections
        .iter()
        .map(|section| {
            let items = section
                .items
                .iter()
                .map(|item| sidebar_item(item.label, item.href, item.href == current_path))
                .collect::<Vec<_>>();
            sidebar_section(section.title, &items)
        })
        .collect()
}

fn desktop_sidebar(sections: &[NavSection], current_path: &str) -> Containers {
    let sections = sidebar_sections(sections, current_path);
    container! {
        aside
            #docs-sidebar-desktop
            direction=column
            width=(if_responsive("tablet").then::<i32>(200).or_else(240))
            min-width=(if_responsive("tablet").then::<i32>(200).or_else(240))
            background=#010409
            border-right="1, #21262d"
            padding-y=24
            overflow-y=auto
            hidden=(if_responsive("mobile").then::<bool>(true).or_else(false))
        {
            @for section in sections {
                (section)
            }
        }
    }
}

fn mobile_sidebar(sections: &[NavSection], current_path: &str) -> Containers {
    let sections = sidebar_sections(sections, current_path);
    container! {
        aside
            #docs-sidebar
            direction=column
            width=280
            min-width=280
            background=#010409
            border-right="1, #21262d"
            padding-y=24
            overflow-y=auto
            position=fixed
            top=0
            left=0
            height=100%
            hidden=true
        {
            div
                #sidebar-close
                direction=row
                justify-content=end
                padding-x=16
                padding-bottom=8
            {
                div
                    fx-click=(ActionType::Multi(vec![
                        ActionType::no_display_by_id(SIDEBAR_ID),
                        ActionType::no_display_by_id(BACKDROP_ID),
                    ]))
                    cursor="pointer"
                    color=(text_muted())
                    font-size=18
                    padding=4
                    user-select="none"
                {
                    "\u{2715}"
                }
            }
            @for section in sections {
                (section)
            }
        }
    }
}

fn backdrop() -> Containers {
    container! {
        div
            #docs-backdrop
            hidden=true
            position=fixed
            top=0
            left=0
            width=100%
            height=100%
            background=#00000080
            fx-click=(ActionType::Multi(vec![
                ActionType::no_display_by_id(SIDEBAR_ID),
                ActionType::no_display_by_id(BACKDROP_ID),
            ]))
        {
        }
    }
}

/// Full-page wrapper with nav bar and base styling.
#[must_use]
pub fn page(slot: &Containers) -> Containers {
    container! {
        div
            width=100%
            height=100%
            position=relative
            direction=column
            color=(text_secondary())
            background=#0d1117
            font-family=(MONO_FONT)
            overflow-x=hidden
            overflow-y=auto
        {
            (nav_bar())
            main flex-grow=1 min-height=0 direction=column {
                (slot)
            }
        }
    }
}

fn home_layout(body: &Containers) -> Containers {
    page(&container! {
        div
            flex-grow=1
            min-height=0
            padding-x=24
            padding-y=48
            align-items=center
            justify-content=center
        {
            div max-width=1100 width=100% direction=column gap=48 {
                (body)
            }
        }
    })
}

fn docs_layout(ctx: &ShellContext<'_>) -> Containers {
    page(&container! {
        div
            #docs-main
            direction=(
                if_responsive("mobile")
                    .then::<LayoutDirection>(LayoutDirection::Column)
                    .or_else(LayoutDirection::Row)
            )
            flex-grow=1
            min-height=0
            position=relative
        {
            (desktop_sidebar(&ctx.sections, ctx.current_path))
            (backdrop())
            (mobile_sidebar(&ctx.sections, ctx.current_path))
            div
                flex-grow=1
                min-height=0
                overflow-y=auto
            {
                div
                    #docs-content
                    padding=(if_responsive("tablet").then::<i32>(24).or_else(48))
                    max-width=900
                {
                    (if let Some(title) = ctx.title {
                        container! {
                            h1
                                #docs-title
                                color=(text_primary())
                                font-size=(if_responsive("mobile").then::<i32>(24).or_else(32))
                                font-family=(MONO_FONT)
                                margin-bottom=24
                                padding-bottom=16
                                border-bottom="1, #21262d"
                            {
                                (title)
                            }
                        }
                    } else {
                        container! { div {} }
                    })
                    div
                        #docs-body
                        color=(text_secondary())
                        font-family=(MONO_FONT)
                        font-size=(if_responsive("mobile").then::<i32>(13).or_else(14))
                        overflow-x=auto
                    {
                        (ctx.body)
                    }
                }
            }
        }
    })
}

/// Bcode docs shell used for every page.
#[allow(clippy::needless_pass_by_value)]
#[must_use]
pub fn shell(ctx: ShellContext<'_>) -> Containers {
    if ctx.current_path == "/" || ctx.current_path == "/home" {
        home_layout(ctx.body)
    } else {
        docs_layout(&ctx)
    }
}
