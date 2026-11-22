use rune::color::hex;
use rune::{Color, El, Icon, TextStyle, el, icon, row, text};

use crate::connection::{ProviderCatalog, ProviderMetadata, Thread};

use super::Colors;

macro_rules! logo {
    ($symbol:ident, $file:literal) => {
        static $symbol: Icon = Icon {
            name: $file,
            view: 24.0,
            body: include_str!(concat!("provider_badge/", $file, ".svg")),
        };
    };
}

logo!(OPENAI, "openai");
logo!(CLAUDE, "claude");
logo!(ANTHROPIC, "anthropic");
logo!(OPENROUTER, "openrouter");
logo!(OLLAMA, "ollama");
logo!(ACP_REGISTRY, "acp-registry");
logo!(CURSOR, "cursor");
logo!(GROK, "grok");
logo!(FX, "fx");
logo!(KIRO, "kiro");
logo!(MUSE, "muse");
logo!(OPENCODE_LIGHT, "opencode-light");
logo!(OPENCODE_DARK, "opencode-dark");
logo!(PI, "pi");
logo!(ANTIGRAVITY, "antigravity");

const BADGE_TEXT: TextStyle = TextStyle::new(7.0, 600.0).lh(8.0);

#[derive(Clone, Copy)]
struct Brand {
    name: &'static str,
    logo: &'static Icon,
    color: Option<&'static str>,
}

fn brand(driver: &str, dark: bool) -> Option<Brand> {
    let neutral = if dark { "#f5f5f5" } else { "#0f0f0f" };
    let (name, logo, color) = match driver {
        "codex" => ("Codex", &OPENAI, None),
        "openai" => ("OpenAI", &OPENAI, None),
        "claudeAgent" => ("Claude", &CLAUDE, Some("#d97757")),
        "anthropic" => ("Anthropic", &ANTHROPIC, None),
        "openrouter" => ("OpenRouter", &OPENROUTER, None),
        "ollama" => ("Ollama", &OLLAMA, None),
        "acpRegistry" => (
            "ACP Registry",
            &ACP_REGISTRY,
            Some(if dark { "#ffffff" } else { "#000000" }),
        ),
        "cursor" => (
            "Cursor",
            &CURSOR,
            Some(if dark { "#edecec" } else { "#26251e" }),
        ),
        "grok" => ("Grok", &GROK, Some(neutral)),
        "fx" => ("fx", &FX, Some(neutral)),
        "kiro" => ("Kiro", &KIRO, None),
        "muse" => ("Muse Code", &MUSE, None),
        "opencode" if dark => ("OpenCode", &OPENCODE_DARK, Some("#f1ecec")),
        "opencode" => ("OpenCode", &OPENCODE_LIGHT, Some("#211e1e")),
        "pi" => ("Pi", &PI, Some(neutral)),
        "antigravity" => ("Antigravity", &ANTIGRAVITY, None),
        _ => return None,
    };
    Some(Brand { name, logo, color })
}

#[derive(Clone, Copy)]
struct Instance<'a> {
    id: &'a str,
    metadata: Option<&'a ProviderMetadata>,
    brand: Option<Brand>,
}

impl<'a> Instance<'a> {
    fn resolve(id: &'a str, catalog: Option<&'a ProviderCatalog>, dark: bool) -> Option<Self> {
        match catalog {
            Some(catalog) => {
                let metadata = catalog.get(id)?;
                Some(Self {
                    id,
                    metadata: Some(metadata),
                    brand: brand(&metadata.driver, dark),
                })
            }
            None => Some(Self {
                id,
                metadata: None,
                brand: Some(brand(id, dark)?),
            }),
        }
    }

    fn display_name(self) -> String {
        let driver = self.brand.map(|brand| brand.name);
        if let Some(metadata) = self.metadata {
            if let Some(name) = metadata.display_name.as_deref().map(str::trim)
                && !name.is_empty()
                && (Some(name) != driver || self.id == metadata.driver)
            {
                return name.into();
            }
            if self.id != metadata.driver {
                return humanized_id(self.id);
            }
        }
        driver.map_or_else(|| humanized_id(self.id), str::to_owned)
    }

    fn accent(self) -> Option<Color> {
        let color = self.metadata?.accent_color.as_deref()?.trim();
        (color.len() == 7).then(|| hex(color)).flatten()
    }

    fn has_account_badge(self, catalog: Option<&ProviderCatalog>) -> bool {
        if self.accent().is_some() {
            return true;
        }
        let (Some(catalog), Some(metadata)) = (catalog, self.metadata) else {
            return false;
        };
        metadata.driver != "acpRegistry"
            && catalog
                .values()
                .filter(|other| other.driver == metadata.driver)
                .filter(|other| !metadata.enabled || other.enabled)
                .nth(1)
                .is_some()
    }
}

fn humanized_id(id: &str) -> String {
    let mut label = String::with_capacity(id.len());
    let mut previous: Option<char> = None;
    for glyph in id.chars() {
        if matches!(glyph, '_' | '-') {
            label.push(' ');
        } else if previous.is_none_or(|letter| matches!(letter, '_' | '-')) {
            label.extend(glyph.to_uppercase());
        } else {
            if glyph.is_uppercase() && previous.is_some_and(char::is_lowercase) {
                label.push(' ');
            }
            label.push(glyph);
        }
        previous = Some(glyph);
    }
    label.trim().to_owned()
}

fn initials(label: &str) -> String {
    let mut words = label
        .split(|glyph: char| !glyph.is_alphanumeric() && !matches!(glyph, '\'' | '’'))
        .map(|word| word.trim_matches(['\'', '’']))
        .filter(|word| !word.is_empty());
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut glyphs = first.chars();
    let initial = glyphs.next().expect("nonempty word");
    let second = words
        .next()
        .and_then(|word| word.chars().next())
        .or_else(|| {
            let next = glyphs.next()?;
            let mut last = initial;
            for current in first.chars().skip(1) {
                if last.is_lowercase() && current.is_uppercase() {
                    return Some(current);
                }
                last = current;
            }
            Some(next)
        });
    Some(initial)
        .into_iter()
        .chain(second)
        .flat_map(char::to_uppercase)
        .collect()
}

fn artwork(instance: Instance<'_>, size: f32, current: bool, secondary: Color) -> El {
    let opacity = if current { 0.6 } else { 0.35 };
    if let Some(brand) = instance.brand {
        let base = brand.color.and_then(hex).unwrap_or(secondary);
        let mark = icon(brand.logo, size).opacity(opacity);
        if current {
            mark.color(base)
        } else {
            let gray = base.luminance();
            mark.color(Color {
                r: gray,
                g: gray,
                b: gray,
                a: 1.0,
            })
            .icon_one_color()
        }
    } else {
        let label = initials(&instance.display_name());
        el().size(size, size)
            .center()
            .child(text(label).font(BADGE_TEXT).color(secondary.alpha(opacity)))
    }
}

pub(super) fn stack(
    thread: &Thread,
    catalog: Option<&ProviderCatalog>,
    colors: Colors,
    secondary: Color,
    dark: bool,
) -> Option<El> {
    let id = thread.providers.current_instance()?;
    let instance = Instance::resolve(id, catalog, dark)?;
    let mut current = el()
        .size(14.0, 14.0)
        .center()
        .child(artwork(instance, 14.0, true, secondary));
    if instance.has_account_badge(catalog) {
        let (fill, label_color) =
            instance
                .accent()
                .map_or((colors.background, colors.muted), |accent| {
                    let lightness = accent.luminance() + 0.05;
                    let ink = if lightness / 0.05 >= 1.05 / lightness {
                        Color::BLACK
                    } else {
                        Color::WHITE
                    };
                    (accent, ink)
                });
        current = current.child(
            el().absolute(5.0, 5.0)
                .size(12.0, 12.0)
                .center()
                .radius(6.0)
                .border(1.0, colors.background)
                .fill(fill)
                .child(
                    text(initials(&instance.display_name()))
                        .font(BADGE_TEXT)
                        .color(label_color),
                ),
        );
    }
    Some(
        row()
            .key(format!("provider-stack-{}", thread.id))
            .center_y()
            .gap(6.0)
            .a11y_hidden()
            .children(
                thread
                    .providers
                    .previous_instances(id)
                    .filter_map(|id| Instance::resolve(id, catalog, dark))
                    .map(|previous| artwork(previous, 12.0, false, secondary)),
            )
            .child(current),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn provider_badge_uses_instance_metadata_instead_of_guessing_from_the_id() {
        let metadata: ProviderMetadata = serde_json::from_value(json!({
            "instanceId": "work-codex", "driver": "claudeAgent",
            "displayName": "Review Agent", "accentColor": "#3366CC", "enabled": true
        }))
        .unwrap();
        let providers = ProviderCatalog::from([(metadata.instance_id.clone(), metadata)]);

        let configured = Instance::resolve("work-codex", Some(&providers), false).unwrap();
        assert_eq!(configured.brand.unwrap().logo.name, "claude");
        assert_eq!(initials(&configured.display_name()), "RA");
        assert!(configured.has_account_badge(Some(&providers)));
        assert!(Instance::resolve("work-codex", None, false).is_none());
        assert!(Instance::resolve("codex", Some(&providers), false).is_none());
        assert_eq!(initials("ClaudeProx"), "CP");
        assert_eq!(initials("Theo's Codex"), "TC");
    }
}
