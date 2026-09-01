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
//!
//! ## Why fallback can't simply be left to `Shaping::Advanced`
//!
//! Because that's where the emoji come from. cosmic-text's macOS fallback chain
//! is `.SF NS`, `Menlo`, **`Apple Color Emoji`**, `Geneva`, `Arial Unicode MS`,
//! walked in order — so any character the first two lack and the emoji font has
//! is drawn as a colour emoji. Claude Code's bullet is `⏺` (U+23FA, a *record
//! button* whose Unicode default presentation happens to be emoji), Menlo
//! doesn't have it, and the result is a cartoon dot in the middle of terminal
//! output.
//!
//! [`Fallback`] takes that decision back: a curated chain of monochrome families
//! is consulted ourselves, and the first one that has the glyph draws it under
//! `Shaping::Basic` — no guessing, no colour. Only when *nothing* in the chain
//! has the character does the cell go to `Shaping::Advanced`, which is the case
//! that genuinely is an emoji (`🔴` exists in no text font on macOS) or a script
//! the chain doesn't reach. So a pasted emoji still renders as one, and a symbol
//! that merely has emoji *presentation* renders as text, in the theme's colours,
//! at the grid's own width.
//!
//! The colour test is structural, not a name list: a face carrying `COLR`,
//! `CBDT`, `sbix` or `SVG` is refused. Adding an emoji family to the chain
//! therefore can't reintroduce emoji.
//!
//! ## The weight has to be one the family actually ships
//!
//! Validating the family *name* is not enough, and the gap is not cosmetic:
//! cosmic-text will only honour a named family through a face whose weight
//! matches the request **exactly**. `fallback/mod.rs` filters candidates on
//! `font_weight_diff == 0` before looking for the family at all, so asking a
//! 500-weight family for weight 400 finds nothing and drops through to the
//! system font — a *proportional* one, on a grid built for fixed cells.
//!
//! That is not a hypothetical. Cozette Vector declares weight 500 (Medium);
//! asking it for `Weight::Normal` drew `.SF NS` at 21.6px per cell while the
//! grid stepped 12px, and asking for `Weight::Bold` drew Menlo. Nothing warned,
//! because the *name* resolved perfectly.
//!
//! So the weight is read from the face fontdb resolves ([`SystemFonts::weights`])
//! rather than assumed, and a bold cell asks for the family's own bold face or
//! keeps the base weight. Never a weight nobody has.

use std::collections::HashSet;

use iced::Font;
use iced::font::{Family, Weight};
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

/// Families consulted, in order, for a character the configured font lacks.
///
/// Monospaced first, so a glyph that has to come from elsewhere still matches the
/// grid's width where possible; then the symbol faces that carry the technical
/// and media-control codepoints TUIs reach for (`⎿`, `⧉`, `⏺`); then broad text
/// coverage; then CJK. Every entry is a monochrome face — and the search verifies
/// that rather than trusting this list.
///
/// Off macOS the chain is the common Linux set. A name that isn't installed is
/// simply skipped, and an empty chain means every uncovered character goes to
/// `Shaping::Advanced` — the behavior before any of this existed.
#[cfg(target_os = "macos")]
const FALLBACK_CHAIN: &[&str] = &[
    "Menlo",
    "Apple Symbols",
    "STIX Two Math",
    "Arial Unicode MS",
    "Hiragino Sans",
    "PingFang SC",
    "Apple SD Gothic Neo",
];
#[cfg(not(target_os = "macos"))]
const FALLBACK_CHAIN: &[&str] = &[
    "DejaVu Sans Mono",
    "Noto Sans Symbols 2",
    "DejaVu Sans",
    "Noto Sans",
    "Noto Sans CJK SC",
];

/// The monochrome fallback chain, resolved against the installed fonts.
///
/// Holds the font database because the lookups are lazy: resolving one character
/// means reading a font file, and reading every chain member up front would mean
/// paying for the 20 MB CJK faces at startup to answer a question nothing has
/// asked yet. Each character is resolved once and cached.
pub struct Fallback {
    db: fontdb::Database,
    /// Chain entries that resolved on this machine, in consultation order.
    chain: Vec<(&'static str, fontdb::ID)>,
    cache: std::sync::Mutex<std::collections::HashMap<char, Option<&'static str>>>,
}

impl std::fmt::Debug for Fallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The database has no useful `Debug`, and printing it would be pages of
        // face records.
        f.debug_struct("Fallback")
            .field("chain", &self.chain.iter().map(|(n, _)| *n).collect::<Vec<_>>())
            .finish()
    }
}

impl Fallback {
    /// Which chain family should draw this character, if any.
    ///
    /// `None` means "not in the chain" — the caller should hand the cell to
    /// `Shaping::Advanced`, which is where genuine emoji and unreached scripts
    /// get resolved.
    pub fn family_for(&self, c: char) -> Option<&'static str> {
        if let Ok(cache) = self.cache.lock()
            && let Some(answer) = cache.get(&c)
        {
            return *answer;
        }
        let answer = self
            .chain
            .iter()
            .find(|(_, id)| self.face_draws(*id, c))
            .map(|(name, _)| *name);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(c, answer);
        }
        answer
    }

    /// Whether this face can draw the character *in monochrome*.
    fn face_draws(&self, id: fontdb::ID, c: char) -> bool {
        self.db
            .with_face_data(id, |data, index| {
                let Ok(face) = ttf_parser::Face::parse(data, index) else {
                    return false;
                };
                // A colour table is what makes a font an emoji font, and it's a
                // far better test than matching on the family name: it can't be
                // fooled by a rename and it doesn't need a list to be kept up to
                // date.
                let tables = face.tables();
                let colour = tables.colr.is_some()
                    || tables.cbdt.is_some()
                    || tables.sbix.is_some()
                    || tables.svg.is_some();
                !colour && face.glyph_index(c).is_some()
            })
            .unwrap_or(false)
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
    /// Where a character the font lacks comes from instead. `None` disables the
    /// monochrome chain, leaving cosmic-text's own fallback — which is what
    /// produces the emoji this exists to prevent.
    pub fallback: Option<&'static Fallback>,
    /// The family's own bold face, if it has one. `None` means bold cells keep
    /// the base weight: asking for a weight the family doesn't ship doesn't get
    /// a heavier version of this font, it gets a *different font* — see the
    /// module docs.
    bold_weight: Option<Weight>,
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

    /// Which family draws a character this font lacks, if the chain has one.
    ///
    /// Only meaningful for characters `can_draw` refused; asking about a covered
    /// one would read font files to answer a question already settled.
    pub fn fallback_family(&self, c: char) -> Option<&'static str> {
        self.fallback?.family_for(c)
    }

    /// Attach coverage read from the system font database.
    pub fn with_coverage(mut self, coverage: Option<&'static Coverage>) -> Self {
        self.coverage = coverage;
        self
    }

    /// Attach the monochrome fallback chain.
    pub fn with_fallback(mut self, fallback: Option<&'static Fallback>) -> Self {
        self.fallback = fallback;
        self
    }

    /// Adopt the weights the family actually ships, from [`SystemFonts::weights`].
    ///
    /// `base` is the weight of the face a normal query resolves to — requesting
    /// anything else by name silently lands in another font entirely.
    pub fn with_weights(mut self, base: Option<Weight>, bold: Option<Weight>) -> Self {
        if let Some(base) = base {
            self.font.weight = base;
        }
        self.bold_weight = bold;
        self
    }

    /// Same font at a different weight/style, for bold and italic cells.
    ///
    /// A bold cell gets the family's own bold face, or the base weight when it
    /// has none. It never gets `Weight::Bold` on spec: an unmatched weight is not
    /// a near miss, it's a different family — see the module docs.
    pub fn variant(&self, bold: bool, italic: bool) -> Font {
        Font {
            weight: match (bold, self.bold_weight) {
                (true, Some(bold)) => bold,
                _ => self.font.weight,
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
            fallback: None,
            bold_weight: None,
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
        let id = self.db.query(&fontdb::Query {
            families: &[db_family(family)],
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

    /// The weights this family actually ships: `(base, bold)`.
    ///
    /// `base` is the weight of the face a normal query resolves to — the one
    /// cosmic-text will draw with — and it must be requested exactly or the
    /// family is skipped entirely (see the module docs). `bold` is the family's
    /// own bold face, preferring a true 700 and otherwise the heaviest upright
    /// face above `base`; `None` when it has nothing heavier.
    ///
    /// The third value is a warning for a face declaring a weight `iced::font`
    /// can't name (say 450). Those exist, and the honest answer is to say so —
    /// silently rounding to the nearest nameable weight is exactly the
    /// wrong-font-on-screen bug this function exists to prevent.
    pub fn weights(&self, family: &Family) -> (Option<Weight>, Option<Weight>, Option<String>) {
        let Some(id) = self.db.query(&fontdb::Query {
            families: &[db_family(family)],
            ..Default::default()
        }) else {
            return (None, None, None);
        };
        let Some(face) = self.db.face(id) else {
            return (None, None, None);
        };
        let base_raw = face.weight.0;
        let Some(name) = face.families.first().map(|(n, _)| n.clone()) else {
            return (None, None, None);
        };

        let Some(base) = iced_weight(base_raw) else {
            return (
                None,
                None,
                Some(format!(
                    "font \"{name}\" declares weight {base_raw}, which can't be requested \
                     exactly — using normal weight, which may draw a different font"
                )),
            );
        };

        // Only upright faces: a bold cell keeps its own style, and an italic-only
        // heavy face would be the wrong shape for it.
        let heavier: Vec<u16> = self
            .db
            .faces()
            .filter(|f| f.style == fontdb::Style::Normal)
            .filter(|f| f.families.iter().any(|(n, _)| *n == name))
            .map(|f| f.weight.0)
            .filter(|w| *w > base_raw)
            .collect();
        // A true 700 first, since that's what "bold" means; otherwise the
        // heaviest thing available, which is better than nothing.
        let bold = heavier
            .contains(&700)
            .then_some(700)
            .or_else(|| heavier.iter().copied().max())
            .and_then(iced_weight);

        (Some(base), bold, None)
    }

    /// Resolve [`FALLBACK_CHAIN`] against what's installed.
    ///
    /// Consumes `self` because the database has to outlive startup: the chain's
    /// per-character lookups happen later, at draw time.
    pub fn into_fallback(self) -> Fallback {
        let chain = FALLBACK_CHAIN
            .iter()
            .filter_map(|name| {
                let id = self.db.query(&fontdb::Query {
                    families: &[fontdb::Family::Name(name)],
                    ..Default::default()
                })?;
                Some((*name, id))
            })
            .collect();
        Fallback {
            db: self.db,
            chain,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

/// The same family, as `fontdb` spells it.
///
/// Shared by [`SystemFonts::coverage`] and [`SystemFonts::weights`] so both ask
/// the database the question cosmic-text will ask, in one place.
fn db_family<'a>(family: &'a Family) -> fontdb::Family<'a> {
    match family {
        Family::Name(n) => fontdb::Family::Name(n),
        Family::Monospace => fontdb::Family::Monospace,
        Family::SansSerif => fontdb::Family::SansSerif,
        Family::Serif => fontdb::Family::Serif,
        Family::Cursive => fontdb::Family::Cursive,
        Family::Fantasy => fontdb::Family::Fantasy,
    }
}

/// A numeric OS/2 weight as an `iced::font::Weight`, or `None` when iced has no
/// name for it.
///
/// Exact only, deliberately. iced maps its enum onto cosmic-text's numbers
/// one-for-one (`iced_graphics::text`), so a name here reaches the shaper as the
/// same number the face declares — which is the only thing that resolves.
fn iced_weight(raw: u16) -> Option<Weight> {
    Some(match raw {
        100 => Weight::Thin,
        200 => Weight::ExtraLight,
        300 => Weight::Light,
        400 => Weight::Normal,
        500 => Weight::Medium,
        600 => Weight::Semibold,
        700 => Weight::Bold,
        800 => Weight::ExtraBold,
        900 => Weight::Black,
        _ => return None,
    })
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

    /// The bug this whole mechanism exists for: asking a family for a weight it
    /// doesn't ship doesn't get a heavier version of that font, it gets a
    /// different font — a proportional one, on a fixed-cell grid.
    #[test]
    fn variant_keeps_the_base_weight_when_the_family_has_no_bold() {
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        let spec = spec.with_weights(Some(Weight::Medium), None);
        assert_eq!(spec.font.weight, Weight::Medium);
        assert_eq!(spec.variant(true, false).weight, Weight::Medium);
        assert_eq!(spec.variant(false, false).weight, Weight::Medium);
    }

    #[test]
    fn variant_uses_the_family_bold_face_when_it_has_one() {
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        let spec = spec.with_weights(Some(Weight::Normal), Some(Weight::Bold));
        assert_eq!(spec.variant(true, false).weight, Weight::Bold);
        assert_eq!(spec.variant(false, false).weight, Weight::Normal);
    }

    /// Italic is a separate axis and must not disturb the weight, or a bold
    /// italic cell asks for a combination nothing has.
    #[test]
    fn italic_does_not_change_the_weight() {
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        let spec = spec.with_weights(Some(Weight::Medium), None);
        assert_eq!(spec.variant(false, true).weight, Weight::Medium);
        assert_eq!(spec.variant(true, true).weight, Weight::Medium);
    }

    /// Exactness is the whole point — a "close enough" mapping here would put the
    /// wrong font on screen with nothing to say why.
    #[test]
    fn iced_weight_names_only_exact_standard_weights() {
        assert_eq!(super::iced_weight(400), Some(Weight::Normal));
        assert_eq!(super::iced_weight(500), Some(Weight::Medium));
        assert_eq!(super::iced_weight(700), Some(Weight::Bold));
        assert_eq!(super::iced_weight(450), None);
        assert_eq!(super::iced_weight(0), None);
    }

    /// Against the real database, since the failure mode was that our own view of
    /// a font disagreed with the one that draws it.
    #[test]
    fn a_real_family_reports_a_nameable_base_weight() {
        let fonts = super::SystemFonts::load();
        let (base, _bold, warning) = fonts.weights(&Family::Monospace);
        assert!(
            base.is_some(),
            "the default monospace family must report a weight we can request"
        );
        assert!(warning.is_none(), "unexpected warning: {warning:?}");
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

    /// Note the bold case needs a family that *has* a bold face. It used to read
    /// `variant(true, ..) == Bold` unconditionally, which is precisely the bug:
    /// on a family shipping one weight that request leaves the family entirely.
    #[test]
    fn variant_switches_weight_and_style() {
        let (spec, _) = resolve(&FontConfig::default(), &installed());
        let spec = spec.with_weights(Some(Weight::Normal), Some(Weight::Bold));
        assert_eq!(spec.variant(true, false).weight, Weight::Bold);
        assert_eq!(spec.variant(false, true).style, iced::font::Style::Italic);
        assert_eq!(spec.variant(false, false).weight, Weight::Normal);
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

    /// The chain resolves a character the configured font may lack, and refuses
    /// to answer with a colour font.
    ///
    /// Both halves matter. `╰` (U+2570) is a rounded box corner Envy Code R
    /// doesn't have and Menlo does, so it must resolve — that's the path that
    /// keeps a TUI's frame drawn in monochrome at the grid's own width. `🔴` is a
    /// genuine emoji that exists in *no* text font on macOS, so it must resolve
    /// to nothing and be left to system shaping. A `Some` there would mean the
    /// colour-table test had stopped working, which is exactly the regression that
    /// puts cartoon dots back in the middle of shell output.
    #[test]
    fn the_fallback_chain_is_monochrome_and_skips_genuine_emoji() {
        let fallback = super::SystemFonts::load().into_fallback();
        assert_eq!(
            fallback.family_for('\u{2570}'),
            Some("Menlo"),
            "the first chain family with the glyph should answer"
        );
        assert_eq!(
            fallback.family_for('\u{1F534}'),
            None,
            "a colour-only glyph must not be answered by a colour font"
        );
        // Cached answers have to match the uncached ones — the cache is keyed by
        // character and consulted before the search.
        assert_eq!(fallback.family_for('\u{2570}'), Some("Menlo"));
        assert_eq!(fallback.family_for('\u{1F534}'), None);
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

