//! Themes embedded with Bcode, in catalog load order.

/// `(id, embedded TOML source)` for every bundled theme.
pub const BUNDLED_THEMES: &[(&str, &str)] = &[
    (
        "terminal-native",
        include_str!("../themes/terminal-native.toml"),
    ),
    (
        "terminal-native-structured",
        include_str!("../themes/terminal-native-structured.toml"),
    ),
    ("bcode-dark", include_str!("../themes/bcode-dark.toml")),
    ("bcode", include_str!("../themes/bcode.toml")),
    ("bcode-light", include_str!("../themes/bcode-light.toml")),
    ("monochrome", include_str!("../themes/monochrome.toml")),
    (
        "high-contrast",
        include_str!("../themes/high-contrast.toml"),
    ),
    ("nord", include_str!("../themes/nord.toml")),
    ("seeded-base", include_str!("../themes/seeded-base.toml")),
    ("amber", include_str!("../themes/palettes/amber.toml")),
    ("aurora", include_str!("../themes/palettes/aurora.toml")),
    (
        "ayu-mirage",
        include_str!("../themes/palettes/ayu-mirage.toml"),
    ),
    ("ayu", include_str!("../themes/palettes/ayu.toml")),
    (
        "blueprint",
        include_str!("../themes/palettes/blueprint.toml"),
    ),
    (
        "catppuccin-frappe",
        include_str!("../themes/palettes/catppuccin-frappe.toml"),
    ),
    (
        "catppuccin-latte",
        include_str!("../themes/palettes/catppuccin-latte.toml"),
    ),
    (
        "catppuccin-macchiato",
        include_str!("../themes/palettes/catppuccin-macchiato.toml"),
    ),
    (
        "catppuccin-mocha",
        include_str!("../themes/palettes/catppuccin-mocha.toml"),
    ),
    ("dracula", include_str!("../themes/palettes/dracula.toml")),
    ("dusk", include_str!("../themes/palettes/dusk.toml")),
    ("ember", include_str!("../themes/palettes/ember.toml")),
    (
        "everforest",
        include_str!("../themes/palettes/everforest.toml"),
    ),
    ("flexoki", include_str!("../themes/palettes/flexoki.toml")),
    (
        "github-dimmed",
        include_str!("../themes/palettes/github-dimmed.toml"),
    ),
    ("github", include_str!("../themes/palettes/github.toml")),
    ("glacier", include_str!("../themes/palettes/glacier.toml")),
    (
        "gruvbox-material",
        include_str!("../themes/palettes/gruvbox-material.toml"),
    ),
    ("gruvbox", include_str!("../themes/palettes/gruvbox.toml")),
    (
        "kanagawa-dragon",
        include_str!("../themes/palettes/kanagawa-dragon.toml"),
    ),
    ("kanagawa", include_str!("../themes/palettes/kanagawa.toml")),
    ("melange", include_str!("../themes/palettes/melange.toml")),
    ("modus", include_str!("../themes/palettes/modus.toml")),
    (
        "monokai-classic",
        include_str!("../themes/palettes/monokai-classic.toml"),
    ),
    ("moss", include_str!("../themes/palettes/moss.toml")),
    (
        "neon-noir",
        include_str!("../themes/palettes/neon-noir.toml"),
    ),
    (
        "night-owl",
        include_str!("../themes/palettes/night-owl.toml"),
    ),
    ("nightfox", include_str!("../themes/palettes/nightfox.toml")),
    ("one", include_str!("../themes/palettes/one.toml")),
    (
        "oxocarbon",
        include_str!("../themes/palettes/oxocarbon.toml"),
    ),
    ("paper", include_str!("../themes/palettes/paper.toml")),
    ("phosphor", include_str!("../themes/palettes/phosphor.toml")),
    (
        "poimandres",
        include_str!("../themes/palettes/poimandres.toml"),
    ),
    (
        "rose-pine-moon",
        include_str!("../themes/palettes/rose-pine-moon.toml"),
    ),
    (
        "rose-pine",
        include_str!("../themes/palettes/rose-pine.toml"),
    ),
    (
        "solarized",
        include_str!("../themes/palettes/solarized.toml"),
    ),
    (
        "tokyonight-moon",
        include_str!("../themes/palettes/tokyonight-moon.toml"),
    ),
    (
        "tokyonight-storm",
        include_str!("../themes/palettes/tokyonight-storm.toml"),
    ),
    (
        "tokyonight",
        include_str!("../themes/palettes/tokyonight.toml"),
    ),
    ("vesper", include_str!("../themes/palettes/vesper.toml")),
    ("zenburn", include_str!("../themes/palettes/zenburn.toml")),
    (
        "accessible",
        include_str!("../themes/profiles/accessible.toml"),
    ),
    ("bare", include_str!("../themes/profiles/bare.toml")),
    (
        "brutalist",
        include_str!("../themes/profiles/brutalist.toml"),
    ),
    ("compact", include_str!("../themes/profiles/compact.toml")),
    (
        "editorial",
        include_str!("../themes/profiles/editorial.toml"),
    ),
    ("focus", include_str!("../themes/profiles/focus.toml")),
    ("minimal", include_str!("../themes/profiles/minimal.toml")),
    ("retro", include_str!("../themes/profiles/retro.toml")),
    ("riced", include_str!("../themes/profiles/riced.toml")),
    ("soft", include_str!("../themes/profiles/soft.toml")),
    (
        "structured",
        include_str!("../themes/profiles/structured.toml"),
    ),
    ("timeline", include_str!("../themes/profiles/timeline.toml")),
    ("zen", include_str!("../themes/profiles/zen.toml")),
    (
        "amber-terminal",
        include_str!("../themes/combos/amber-terminal.toml"),
    ),
    (
        "blueprint-studio",
        include_str!("../themes/combos/blueprint-studio.toml"),
    ),
    (
        "brutal-modus",
        include_str!("../themes/combos/brutal-modus.toml"),
    ),
    ("crt", include_str!("../themes/combos/crt.toml")),
    (
        "forest-floor",
        include_str!("../themes/combos/forest-floor.toml"),
    ),
    ("neon", include_str!("../themes/combos/neon.toml")),
    ("newsroom", include_str!("../themes/combos/newsroom.toml")),
    (
        "night-shift",
        include_str!("../themes/combos/night-shift.toml"),
    ),
    ("quiet", include_str!("../themes/combos/quiet.toml")),
    (
        "rice-mocha",
        include_str!("../themes/combos/rice-mocha.toml"),
    ),
    ("zen-paper", include_str!("../themes/combos/zen-paper.toml")),
];
