//! Bundled palettes reproduce their upstream color schemes exactly.
//!
//! The tests are driven by data in `themes/attribution`: `sources.toml`
//! classifies every palette, and `upstream/<id>.toml` records an upstream
//! palette's source, revision, license, and the upstream values its palette
//! slots and roles must equal. Converting a palette adds data only; see
//! `themes/attribution/UPSTREAM.md`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use bcode_tui_theme::definition::{
    ResolvedThemeDefinition, ResolvedThemeVariant, ThemeCatalog, ThemeKind, ThemeSelection,
};
use bmux_tui::style::Color;
use serde::Deserialize;

/// Where a palette's colors come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Upstream,
    Pending,
    Original,
    Measured,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    status: Status,
    #[serde(default)]
    family: Option<String>,
    #[serde(default)]
    candidates: Vec<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sources {
    palettes: BTreeMap<String, Source>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    palette: String,
    scheme: String,
    source: String,
    revision: String,
    license: String,
    variants: BTreeMap<String, VariantReference>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantReference {
    /// Upstream file the values were read from, relative to `source`.
    file: String,
    /// Upstream name → `#rrggbb`, exactly as upstream defines it.
    colors: BTreeMap<String, String>,
    /// Bcode palette entry → upstream name.
    slots: BTreeMap<String, String>,
    /// Bcode role color (`role.fg` or `role.bg`) → upstream name.
    #[serde(default)]
    roles: BTreeMap<String, String>,
}

fn attribution() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/attribution")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Ids of the TOML files directly in `dir`.
fn toml_ids(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .map(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .expect("utf-8 file name")
                .to_owned()
        })
        .collect()
}

fn sources() -> Sources {
    let path = attribution().join("sources.toml");
    toml::from_str(&read(&path)).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn references() -> BTreeMap<String, Reference> {
    let dir = attribution().join("upstream");
    toml_ids(&dir)
        .into_iter()
        .map(|id| {
            let path = dir.join(format!("{id}.toml"));
            let reference = toml::from_str(&read(&path))
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
            (id, reference)
        })
        .collect()
}

fn hex(color: Option<Color>) -> Option<String> {
    color.and_then(Color::rgb).map(|rgb| rgb.to_hex())
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn variant(name: &str) -> Option<ResolvedThemeVariant> {
    match name {
        "dark" => Some(ResolvedThemeVariant::Dark),
        "light" => Some(ResolvedThemeVariant::Light),
        _ => None,
    }
}

#[test]
fn every_palette_declares_where_its_colors_come_from() {
    let palettes = toml_ids(&Path::new(env!("CARGO_MANIFEST_DIR")).join("themes/palettes"));
    let sources = sources();
    let references = references();
    let classified: BTreeSet<String> = sources.palettes.keys().cloned().collect();
    assert_eq!(
        palettes, classified,
        "themes/palettes and sources.toml list the same ids"
    );
    let mut problems = Vec::new();
    for (id, source) in &sources.palettes {
        let has_reference = references.contains_key(id);
        match source.status {
            Status::Upstream if !has_reference => {
                problems.push(format!("{id}: upstream without upstream/{id}.toml"));
            }
            Status::Pending if source.family.is_none() || source.candidates.is_empty() => {
                problems.push(format!("{id}: pending needs a family and candidates"));
            }
            Status::Measured if source.note.is_none() => {
                problems.push(format!("{id}: measured needs a note"));
            }
            status if status != Status::Upstream && has_reference => {
                problems.push(format!("{id}: has a reference but status {status:?}"));
            }
            _ => {}
        }
    }
    for id in references.keys().filter(|id| !classified.contains(*id)) {
        problems.push(format!("upstream/{id}.toml names no bundled palette"));
    }
    assert_eq!(problems, Vec::<String>::new());
}

/// Problems with one variant of an upstream palette.
fn variant_problems(
    id: &str,
    name: &str,
    reference: &VariantReference,
    resolved: &ResolvedThemeDefinition,
) -> Vec<String> {
    let label = format!("{id} {name}");
    let mut problems = Vec::new();
    if reference.file.trim().is_empty() {
        problems.push(format!("{label}: `file` is empty"));
    }
    for (upstream, value) in &reference.colors {
        if !is_hex_color(value) {
            problems.push(format!("{label}: {upstream} = {value:?} is not #rrggbb"));
        }
    }
    let upstream_value = |upstream: &str| reference.colors.get(upstream).map(String::as_str);
    // Every color the palette declares is traceable to an upstream value.
    let declared: BTreeSet<&str> = resolved.palette.keys().map(String::as_str).collect();
    let mapped: BTreeSet<&str> = reference.slots.keys().map(String::as_str).collect();
    for slot in declared.difference(&mapped) {
        problems.push(format!(
            "{label}: palette entry `{slot}` has no upstream slot mapping"
        ));
    }
    for (slot, upstream) in &reference.slots {
        let Some(expected) = upstream_value(upstream) else {
            problems.push(format!(
                "{label}: slot {slot} maps unknown upstream {upstream}"
            ));
            continue;
        };
        let actual = hex(resolved.color(slot));
        if actual.as_deref() != Some(expected) {
            problems.push(format!(
                "{label}: ${slot} is {actual:?}, upstream {upstream} is {expected}"
            ));
        }
    }
    for (target, upstream) in &reference.roles {
        let Some(expected) = upstream_value(upstream) else {
            problems.push(format!(
                "{label}: role {target} maps unknown upstream {upstream}"
            ));
            continue;
        };
        let Some((role, channel)) = target.rsplit_once('.') else {
            problems.push(format!("{label}: role {target} lacks .fg or .bg"));
            continue;
        };
        let style = resolved.style(role);
        let actual = match channel {
            "fg" => hex(style.and_then(|style| style.fg)),
            "bg" => hex(style.and_then(|style| style.bg)),
            _ => {
                problems.push(format!("{label}: role {target} lacks .fg or .bg"));
                continue;
            }
        };
        if actual.as_deref() != Some(expected) {
            problems.push(format!(
                "{label}: {target} is {actual:?}, upstream {upstream} is {expected}"
            ));
        }
    }
    problems
}

#[test]
fn upstream_palettes_resolve_to_their_reference_colors() {
    let catalog = ThemeCatalog::bundled().expect("bundled catalog");
    let references = references();
    let mut problems = Vec::new();
    let mut checked = 0;
    for (id, reference) in &references {
        if reference.palette != *id {
            problems.push(format!(
                "upstream/{id}.toml names palette {}",
                reference.palette
            ));
        }
        for (field, value) in [
            ("scheme", &reference.scheme),
            ("source", &reference.source),
            ("license", &reference.license),
        ] {
            if value.trim().is_empty() {
                problems.push(format!("{id}: `{field}` is empty"));
            }
        }
        // A moving branch name would not pin the values that were read.
        if reference.revision.trim().is_empty()
            || ["main", "master", "HEAD", "trunk", "develop"].contains(&reference.revision.as_str())
        {
            problems.push(format!("{id}: `revision` must pin a commit or tag"));
        }
        let Some(definition) = catalog.definition(id) else {
            problems.push(format!("{id}: no bundled theme"));
            continue;
        };
        if reference.variants.is_empty() {
            problems.push(format!("{id}: no variants"));
        }
        for (name, variant_reference) in &reference.variants {
            let Some(resolved_variant) = variant(name) else {
                problems.push(format!("{id}: unknown variant {name}"));
                continue;
            };
            if resolved_variant == ResolvedThemeVariant::Light && !definition.has_light_variant() {
                problems.push(format!(
                    "{id}: references a light variant it does not define"
                ));
                continue;
            }
            let resolved = catalog
                .resolve(&ThemeSelection::new(id.clone()).variant(resolved_variant))
                .unwrap_or_else(|error| panic!("{id} {name}: {error}"));
            problems.extend(variant_problems(id, name, variant_reference, &resolved));
            checked += 1;
        }
        if definition.has_light_variant() && !reference.variants.contains_key("light") {
            problems.push(format!("{id}: the light variant has no reference"));
        }
    }
    assert_eq!(problems, Vec::<String>::new());
    assert!(checked > 0, "at least one upstream palette is referenced");
}

/// Every role color a palette slot feeds is that slot's color exactly, in
/// every bundled theme and variant.
#[test]
fn palette_slot_roles_keep_their_slot_color() {
    let catalog = ThemeCatalog::bundled().expect("bundled catalog");
    let mut kept = 0;
    for definition in catalog
        .definitions()
        .filter(|definition| definition.kind() == ThemeKind::Theme)
    {
        for variant in [ResolvedThemeVariant::Dark, ResolvedThemeVariant::Light] {
            let resolved = catalog
                .resolve(&ThemeSelection::new(definition.id()).variant(variant))
                .unwrap_or_else(|error| panic!("{}: {error}", definition.id()));
            for color in &resolved.palette_roles {
                let style = resolved.style(&color.role).expect("palette role is set");
                let actual = match color.channel {
                    bcode_tui_theme::contrast::AdjustedChannel::Foreground => style.fg,
                    bcode_tui_theme::contrast::AdjustedChannel::Background => style.bg,
                };
                assert_eq!(
                    actual,
                    resolved.color(&color.slot),
                    "{} {variant:?} {color:?}",
                    definition.id()
                );
                kept += 1;
            }
        }
    }
    assert!(kept > 0, "bundled palettes keep slot colors");
}
