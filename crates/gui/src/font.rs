//! Turning `[font]` config into the font values the renderer wants.
//!
//! ## The `&'static str` problem
//!
//! `iced::font::Family::Name` holds a `&'static str`, but a family name read
//! from `config.toml` is a runtime `String`. The options are to leak it or to
//! carry a lifetime through every widget that draws text. We leak: it happens
//! once at startup, for a string that must live as long as the process anyway,
//! and the alternative poisons the whole widget tree with a lifetime parameter
//! for no benefit. `Box::leak` is the honest way to say that.
//!
//! ## Unknown family names
//!
//! `Shaping::Basic` disables font fallback — that's what makes it cheap, and
//! it's the right default for a grid whose font we control. The cost is that
//! naming a family that isn't installed has no safety net at shaping time. So
//! the family is validated up front against the fonts iced actually loaded, and
//! an unknown name falls back to the default monospace with a status message
//! rather than silently rendering nothing.
//!
//! ## Missing *glyphs*, as opposed to missing families
//!
//! Validating the family isn't enough, because a font that resolves perfectly
//! can still lack individual characters — and with no fallback those cells draw
//! nothing at all. This is not a corner case: Envy Code R covers 48 of the 160
//! codepoints in the box-drawing block, missing every heavy, dashed and
//! rounded-corner variant, so any TUI drawing a frame with `╭─╮` loses its
//! corners silently.
//!
//! [`Coverage`] is the answer: read the resolved face's `cmap` once at startup
//! and record which characters it can actually draw. `GridView` consults it per
//! cell and shapes the few uncovered ones with `Shaping::Advanced`, which *does*
//! fall back to system fonts. Cheap path stays cheap; missing glyphs appear.

use std::collections::HashSet;

use iced::Font;
use iced::font::Family;
use sacrament_core::font::FontConfig;

/// Which characters the resolved font can draw, from its `cmap`.
///
/// ASCII gets a flat array rather than the hash set: it's the overwhelming
/// majority of cells and this is consulted once per cell per frame, so the
/// common case shouldn't hash.
#[derive(Debug)]
pub struct Coverage {
    ascii: [bool; 128],
    other: HashSet<char>,
}

impl Coverage {
    fn empty() -> Self {
        Self {
            ascii: [false; 128],
            other: HashSet::new(),
        }
    }

    fn insert(&mut self, c: char) {
        let cp = c as usize;
        if cp < 128 {
            self.ascii[cp] = true;
        } else {
            self.other.insert(c);
        }
    }

    pub fn covers(&self, c: char) -> bool {
        let cp = c as usize;
        if cp < 128 {
            self.ascii[cp]
        } else {
            self.other.contains(&c)
        }
    }

    /// Build coverage from an explicit character list, for tests.
    #[cfg(test)]
    pub fn from_chars(chars: &[char]) -> Self {
        let mut cov = Self::empty();
        for c in chars {
            cov.insert(*c);
        }
        cov
    }

    /// Whether nothing at all is covered. A face that parsed but maps no
    /// characters means something went wrong, and treating it as unknown is
    /// better than routing every cell through fallback shaping.
    fn is_empty(&self) -> bool {
        self.other.is_empty() && !self.ascii.iter().any(|b| *b)
    }
}

/// Font settings resolved into renderer-native values, once at startup.
#[derive(Clone, Copy, Debug)]
pub struct FontSpec {
    pub font: Font,
    pub size: f32,
    pub line_height: f32,
    /// `None` means coverage couldn't be determined, in which case every
    /// character is assumed drawable — the pre-existing behavior, so a font we
    /// can't introspect is no worse off than before.
    ///
    /// `&'static` keeps `FontSpec` `Copy`; it's leaked once at startup for the
    /// same reason the family name is (see the module docs).
    pub coverage: Option<&'static Coverage>,
}

impl FontSpec {
    /// Cell height in pixels. The grid's row pitch.
    pub fn cell_height(&self) -> f32 {
        self.size * self.line_height
    }

    /// Whether the font can draw this character itself, or needs fallback.
    ///
    /// Bold and italic faces are judged by the regular face's coverage. They can
    /// differ in principle, but a font shipping box-drawing in one weight and not
    /// another is pathological, and checking all four would mean parsing four
    /// faces to catch it.
    pub fn can_draw(&self, c: char) -> bool {
        match self.coverage {
            Some(cov) => cov.covers(c),
            None => true,
        }
    }

    /// Attach coverage read from the system font database.
    pub fn with_coverage(mut self, coverage: Option<&'static Coverage>) -> Self {
        self.coverage = coverage;
        self
    }

    /// Same font at a different weight/style, for bold and italic cells.
    pub fn variant(&self, bold: bool, italic: bool) -> Font {
        Font {
            weight: if bold {
                iced::font::Weight::Bold
            } else {
                iced::font::Weight::Normal
            },
            style: if italic {
                iced::font::Style::Italic
            } else {
                iced::font::Style::Normal
            },
            ..self.font
        }
    }
}

/// Resolve config into a [`FontSpec`], plus a warning when the requested family
/// couldn't be used.
pub fn resolve(cfg: &FontConfig, available: &[String]) -> (FontSpec, Option<String>) {
    let cfg = cfg.sanitized();
    let (family, warning) = resolve_family(cfg.family.as_deref(), available);
    (
        FontSpec {
            font: Font {
                family,
                ..Font::MONOSPACE
            },
            size: cfg.size,
            line_height: cfg.line_height,
            coverage: None,
        },
        warning,
    )
}

/// The system font database, loaded once.
///
/// Uses `fontdb`, which is the same crate (and version) cosmic-text loads system
/// fonts through, so what it reports is exactly what iced can actually resolve —
/// not an approximation from a separate source like `system_profiler`. Loading it
/// is not free, hence one instance shared by family validation and coverage
/// rather than one each.
pub struct SystemFonts {
    db: fontdb::Database,
}

impl SystemFonts {
    pub fn load() -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        Self { db }
    }

    /// Every installed family name, sorted and deduplicated.
    pub fn families(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .db
            .faces()
            .flat_map(|f| f.families.iter().map(|(name, _)| name.clone()))
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Read the `cmap` of the face this family resolves to.
    ///
    /// `None` when the family can't be resolved or the face can't be parsed, in
    /// which case callers assume full coverage — degrading to the old behavior
    /// beats refusing to draw.
    pub fn coverage(&self, family: &Family) -> Option<Coverage> {
        // Query the same way cosmic-text will, so coverage describes the face
        // that actually gets used rather than a different one in the family.
        let name = match family {
            Family::Name(n) => fontdb::Family::Name(n),
            Family::Monospace => fontdb::Family::Monospace,
            Family::SansSerif => fontdb::Family::SansSerif,
            Family::Serif => fontdb::Family::Serif,
            Family::Cursive => fontdb::Family::Cursive,
            Family::Fantasy => fontdb::Family::Fantasy,
        };
        let id = self.db.query(&fontdb::Query {
            families: &[name],
            ..Default::default()
        })?;
        // `with_face_data` returns `Option<T>` for "no such face", and the closure
        // returns `Option<Coverage>` for "unusable face", so this is doubly
        // optional — hence the `?` plus the flatten.
        self.db.with_face_data(id, |data, index| {
            let face = ttf_parser::Face::parse(data, index).ok()?;
            let mut cov = Coverage::empty();
            for sub in face.tables().cmap?.subtables {
                if !sub.is_unicode() {
                    continue;
                }
                // `codepoints` yields the subtable's *candidates*; a candidate can
                // still map to no glyph, so each is confirmed against
                // `glyph_index` rather than trusted.
                sub.codepoints(|cp| {
                    if let Some(c) = char::from_u32(cp)
                        && face.glyph_index(c).is_some()
                    {
                        cov.insert(c);
                    }
                });
            }
            // A face that parsed but covers nothing means something went wrong;
            // treat it as unknown so we don't route every cell through fallback.
            (!cov.is_empty()).then_some(cov)
        })?
    }
}

/// Match a requested family against what's installed, case-insensitively.
///
/// Returns `Family::Monospace` and a warning when the name doesn't match, since
/// with `Shaping::Basic` an unresolvable family would otherwise draw nothing.
fn resolve_family(requested: Option<&str>, available: &[String]) -> (Family, Option<String>) {
    let Some(name) = requested else {
        return (Family::Monospace, None);
    };
    match available.iter().find(|f| f.eq_ignore_ascii_case(name)) {
        // Leak deliberately — see the module docs.
        Some(found) => (Family::Name(Box::leak(found.clone().into_boxed_str())), None),
        None => (
            Family::Monospace,
            Some(format!("font \"{name}\" not found — using default monospace")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installed() -> Vec<String> {
        vec!["Fira Code".to_string(), "Menlo".to_string()]
    }

    #[test]
    fn unset_family_uses_monospace_without_warning() {
        let (spec, warn) = resolve(&FontConfig::default(), &installed());
        assert!(matches!(spec.font.family, Family::Monospace));
        assert!(warn.is_none());
    }

    #[test]
    fn known_family_resolves() {
        let cfg = FontConfig {
            family: Some("Fira Code".into()),
            ..Default::default()
        };
        let (spec, warn) = resolve(&cfg, &installed());
        assert!(matches!(spec.font.family, Family::Name("Fira Code")));
        assert!(warn.is_none());
    }

    #[test]
    fn family_match_is_case_insensitive() {
        let cfg = FontConfig {
            family: Some("fira code".into()),
            ..Default::default()
        };
        let (spec, _) = resolve(&cfg, &installed());
        assert!(matches!(spec.font.family, Family::Name("Fira Code")));
    }

    #[test]
    fn unknown_family_falls_back_and_warns() {
        let cfg = FontConfig {
            family: Some("Nonexistent Mono".into()),
            ..Default::default()
        };
        let (spec, warn) = resolve(&cfg, &installed());
        assert!(matches!(spec.font.family, Family::Monospace));
        assert!(warn.unwrap().contains("Nonexistent Mono"));
    }

    #[test]
    fn degenerate_size_is_clamped_before_use() {
        let cfg = FontConfig {
            size: 0.0,
            line_height: 0.0,
            ..Default::default()
        };
        let (spec, _) = resolve(&cfg, &installed());
        assert!(spec.cell_height() > 0.0);
    }

    #[test]
    fn without_coverage_every_character_is_assumed_drawable() {
        // The pre-existing behavior. A font we can't introspect must not route
        // every cell through the expensive shaping path.
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        assert!(spec.coverage.is_none());
        assert!(spec.can_draw('a'));
        assert!(spec.can_draw('\u{2570}'));
    }

    #[test]
    fn coverage_decides_which_characters_need_fallback() {
        // `─` present, `╰` absent — exactly the split that made a TUI's box
        // corners disappear while its straight edges drew fine.
        let cov: &'static Coverage =
            Box::leak(Box::new(Coverage::from_chars(&['a', '\u{2500}'])));
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        let spec = spec.with_coverage(Some(cov));
        assert!(spec.can_draw('a'));
        assert!(spec.can_draw('\u{2500}'), "─ is in the font");
        assert!(!spec.can_draw('\u{2570}'), "╰ is not, so it needs fallback");
        assert!(!spec.can_draw('b'), "ascii is not special-cased into coverage");
    }

    #[test]
    fn variant_switches_weight_and_style() {
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        assert_eq!(spec.variant(true, false).weight, iced::font::Weight::Bold);
        assert_eq!(spec.variant(false, true).style, iced::font::Style::Italic);
        assert_eq!(spec.variant(false, false).weight, iced::font::Weight::Normal);
    }
}

#[cfg(test)]
mod system_tests {
    /// Not an assertion about any particular font — just proves the enumeration
    /// path works and returns a plausible list on this machine.
    #[test]
    fn enumerates_installed_families() {
        let families = super::SystemFonts::load().families();
        assert!(!families.is_empty(), "no system fonts found");
        assert!(families.iter().any(|f| f.eq_ignore_ascii_case("Menlo")));
    }

    /// Keeps the `cmap` parse honest. If it silently returned `None` forever,
    /// coverage would degrade to "everything is drawable" and the missing-glyph
    /// fallback would quietly stop working — invisible without this.
    #[test]
    fn coverage_of_a_real_face_is_readable_and_discriminating() {
        let fonts = super::SystemFonts::load();
        let cov = fonts
            .coverage(&iced::font::Family::Monospace)
            .expect("default monospace should parse");
        assert!(cov.covers('x'), "a monospace face must have 'x'");
        assert!(cov.covers('0'));
        // An unassigned codepoint proves the answer isn't a blanket `true`.
        assert!(!cov.covers('\u{10fffd}'));
    }
}

/// Measure the configured font's advance width in pixels.
///
/// Both `GridView` and `Gutter` call this rather than measuring independently.
/// It's deterministic for a given `FontSpec`, so two callers would agree anyway
/// — but having one implementation means row/column alignment between the gutter
/// and the text is a property of the code, not a coincidence to re-verify.
///
/// Row *height* deliberately isn't measured at all: it's `size * line_height`,
/// pure arithmetic, which is what guarantees the two widgets share a row pitch.
pub fn advance_width<Renderer>(_renderer: &Renderer, font: &FontSpec) -> f32
where
    Renderer: iced::advanced::text::Renderer,
    Renderer::Font: From<iced::Font>,
{
    use iced::advanced::text::{self, Paragraph as _};

    // A run of identical glyphs divided by its length gives the advance width
    // without depending on any single glyph's side bearings.
    const SAMPLE: &str = "MMMMMMMMMM";
    let paragraph = Renderer::Paragraph::with_text(text::Text {
        content: SAMPLE,
        bounds: iced::Size::INFINITE,
        size: font.size.into(),
        line_height: text::LineHeight::Relative(font.line_height),
        font: font.font.into(),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    });
    (paragraph.min_bounds().width / SAMPLE.chars().count() as f32).max(1.0)
}

