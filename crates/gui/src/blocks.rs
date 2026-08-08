//! Block-element characters drawn as rectangles instead of glyphs.
//!
//! U+2580..259F are solid fills that are *supposed* to tile — a run of `█`
//! should be one unbroken bar. Drawn as font glyphs they never quite do, and the
//! reason is compositing rather than the font: cell boundaries land on fractional
//! pixels (the advance is `size * 0.537`, the row pitch `size * line_height`,
//! neither integral for any usable size), so at a shared boundary two glyphs each
//! contribute partial coverage. Source-over composites them sequentially rather
//! than summing:
//!
//! ```text
//! a + b(1 - a)  =  0.5 + 0.5(0.5)  =  0.75
//! ```
//!
//! Two half-covered edges give 75% ink, so 25% of the background shows through as
//! a hairline. At scale 1 there is no supersampling to hide it. It's subtle, it
//! appears in slow bands as the fractional part drifts across the grid, and no
//! font can fix it — Envy Code R already gives its blocks a 6-unit horizontal
//! overhang for exactly this, which isn't nearly enough.
//!
//! Every serious terminal (Alacritty, kitty, WezTerm, Ghostty) draws these
//! procedurally for the same reason. This is that.
//!
//! **Snapping is what makes it work, not merging.** Each rect edge is derived from
//! a *shared* cell boundary rounded to a whole pixel, so cell `k`'s right edge and
//! cell `k+1`'s left edge are the same expression and agree by construction — they
//! abut exactly, with no gap and no second blend. Merging adjacent cells is then
//! only an optimisation. The cost is that cells come out 7px or 8px wide rather
//! than a uniform 7.5; invisible on a featureless rectangle, which is precisely why
//! only these characters take this path and text does not.

/// A rectangle inside one cell, as fractions of its width and height:
/// `(x0, y0, x1, y1)` with the origin at the cell's top-left.
pub type Frac = (f32, f32, f32, f32);

const FULL: &[Frac] = &[(0.0, 0.0, 1.0, 1.0)];

// Eighths, counted from the edge the character grows from. `▁` is one eighth at
// the bottom, `▇` is seven.
const LOWER_1: &[Frac] = &[(0.0, 7.0 / 8.0, 1.0, 1.0)];
const LOWER_2: &[Frac] = &[(0.0, 6.0 / 8.0, 1.0, 1.0)];
const LOWER_3: &[Frac] = &[(0.0, 5.0 / 8.0, 1.0, 1.0)];
const LOWER_4: &[Frac] = &[(0.0, 4.0 / 8.0, 1.0, 1.0)];
const LOWER_5: &[Frac] = &[(0.0, 3.0 / 8.0, 1.0, 1.0)];
const LOWER_6: &[Frac] = &[(0.0, 2.0 / 8.0, 1.0, 1.0)];
const LOWER_7: &[Frac] = &[(0.0, 1.0 / 8.0, 1.0, 1.0)];

const LEFT_7: &[Frac] = &[(0.0, 0.0, 7.0 / 8.0, 1.0)];
const LEFT_6: &[Frac] = &[(0.0, 0.0, 6.0 / 8.0, 1.0)];
const LEFT_5: &[Frac] = &[(0.0, 0.0, 5.0 / 8.0, 1.0)];
const LEFT_4: &[Frac] = &[(0.0, 0.0, 4.0 / 8.0, 1.0)];
const LEFT_3: &[Frac] = &[(0.0, 0.0, 3.0 / 8.0, 1.0)];
const LEFT_2: &[Frac] = &[(0.0, 0.0, 2.0 / 8.0, 1.0)];
const LEFT_1: &[Frac] = &[(0.0, 0.0, 1.0 / 8.0, 1.0)];

const UPPER_HALF: &[Frac] = &[(0.0, 0.0, 1.0, 0.5)];
const RIGHT_HALF: &[Frac] = &[(0.5, 0.0, 1.0, 1.0)];
const UPPER_1: &[Frac] = &[(0.0, 0.0, 1.0, 1.0 / 8.0)];
const RIGHT_1: &[Frac] = &[(7.0 / 8.0, 0.0, 1.0, 1.0)];

// Quadrants. The three-quadrant characters are expressed as a half plus the one
// remaining quadrant rather than as three squares, so their rects never overlap —
// which is what lets the tests assert coverage by summing areas.
const Q_UL: &[Frac] = &[(0.0, 0.0, 0.5, 0.5)];
const Q_UR: &[Frac] = &[(0.5, 0.0, 1.0, 0.5)];
const Q_LL: &[Frac] = &[(0.0, 0.5, 0.5, 1.0)];
const Q_LR: &[Frac] = &[(0.5, 0.5, 1.0, 1.0)];
const Q_UL_LR: &[Frac] = &[(0.0, 0.0, 0.5, 0.5), (0.5, 0.5, 1.0, 1.0)];
const Q_UR_LL: &[Frac] = &[(0.5, 0.0, 1.0, 0.5), (0.0, 0.5, 0.5, 1.0)];
const Q_UL_LL_LR: &[Frac] = &[(0.0, 0.0, 0.5, 0.5), (0.0, 0.5, 1.0, 1.0)];
const Q_UL_UR_LL: &[Frac] = &[(0.0, 0.0, 1.0, 0.5), (0.0, 0.5, 0.5, 1.0)];
const Q_UL_UR_LR: &[Frac] = &[(0.0, 0.0, 1.0, 0.5), (0.5, 0.5, 1.0, 1.0)];
const Q_UR_LL_LR: &[Frac] = &[(0.5, 0.0, 1.0, 0.5), (0.0, 0.5, 1.0, 1.0)];

/// The rectangles this character is made of, or `None` to draw it as a glyph.
///
/// The shades `░▒▓` (U+2591..2593) deliberately return `None`. They are 25/50/75%
/// stipple, so a rect version would need either a real dot pattern or a blended
/// colour — and a blend produces a colour the user's `[theme]` doesn't contain,
/// which `theme_guard` rejects. They also seam far less visibly, not being solid.
pub fn rects(c: char) -> Option<&'static [Frac]> {
    Some(match c {
        '\u{2580}' => UPPER_HALF,
        '\u{2581}' => LOWER_1,
        '\u{2582}' => LOWER_2,
        '\u{2583}' => LOWER_3,
        '\u{2584}' => LOWER_4,
        '\u{2585}' => LOWER_5,
        '\u{2586}' => LOWER_6,
        '\u{2587}' => LOWER_7,
        '\u{2588}' => FULL,
        '\u{2589}' => LEFT_7,
        '\u{258A}' => LEFT_6,
        '\u{258B}' => LEFT_5,
        '\u{258C}' => LEFT_4,
        '\u{258D}' => LEFT_3,
        '\u{258E}' => LEFT_2,
        '\u{258F}' => LEFT_1,
        '\u{2590}' => RIGHT_HALF,
        // 2591..2593 are the shades — glyphs, see above.
        '\u{2594}' => UPPER_1,
        '\u{2595}' => RIGHT_1,
        '\u{2596}' => Q_LL,
        '\u{2597}' => Q_LR,
        '\u{2598}' => Q_UL,
        '\u{2599}' => Q_UL_LL_LR,
        '\u{259A}' => Q_UL_LR,
        '\u{259B}' => Q_UL_UR_LL,
        '\u{259C}' => Q_UL_UR_LR,
        '\u{259D}' => Q_UR,
        '\u{259E}' => Q_UR_LL,
        '\u{259F}' => Q_UR_LL_LR,
        _ => return None,
    })
}

/// Whether this character tiles horizontally as a single full-width rect, so a run
/// of them can be drawn as one quad.
///
/// Purely an optimisation — snapping already makes per-cell rects abut exactly.
/// It matters for block-heavy output (progress bars, charts, a large logo), where
/// the alternative is one quad per cell.
pub fn merges_horizontally(c: char) -> Option<(f32, f32)> {
    match rects(c) {
        Some([(x0, y0, x1, y1)]) if *x0 == 0.0 && *x1 == 1.0 => Some((*y0, *y1)),
        _ => None,
    }
}

/// One edge of a rect, in pixels, between two already-snapped cell boundaries.
///
/// The endpoints are returned untouched so an edge that *is* a cell boundary keeps
/// the exact value its neighbour will use. Interior fractions are rounded, which
/// is what makes `▀` and `▄` in the same cell meet on one pixel rather than
/// straddling it.
pub fn edge(low: f32, high: f32, t: f32) -> f32 {
    if t <= 0.0 {
        low
    } else if t >= 1.0 {
        high
    } else {
        (low + (high - low) * t).round()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every block character in the range, minus the three shades.
    fn drawn() -> Vec<char> {
        (0x2580..=0x259F)
            .filter_map(char::from_u32)
            .filter(|c| rects(*c).is_some())
            .collect()
    }

    fn area(c: char) -> f32 {
        rects(c)
            .unwrap()
            .iter()
            .map(|(x0, y0, x1, y1)| (x1 - x0) * (y1 - y0))
            .sum()
    }

    #[test]
    fn the_shades_stay_glyphs() {
        // Not an oversight: 25/50/75% stipple would need a blended colour, which
        // is a colour the theme doesn't contain — `theme_guard` fails on that.
        for c in ['\u{2591}', '\u{2592}', '\u{2593}'] {
            assert!(rects(c).is_none(), "U+{:04X} should stay a glyph", c as u32);
        }
        // And everything else in the range is covered.
        assert_eq!(drawn().len(), 32 - 3);
    }

    #[test]
    fn nothing_outside_the_block_range_is_claimed() {
        for c in ['a', ' ', '\0', '─', '│', '┼', '⏺', '中', '\u{257F}', '\u{25A0}'] {
            assert!(rects(c).is_none(), "{c:?} is not a block element");
        }
    }

    #[test]
    fn halves_tile_into_a_full_block() {
        // The property the whole exercise exists for: two halves must cover the
        // cell exactly, with no seam between them and nothing sticking out.
        let (_, _, _, top_bottom) = rects('\u{2580}').unwrap()[0];
        let (_, bottom_top, _, _) = rects('\u{2584}').unwrap()[0];
        assert_eq!(top_bottom, bottom_top, "upper and lower half must meet");
        assert_eq!(area('\u{2580}') + area('\u{2584}'), 1.0);

        let (_, _, left_right, _) = rects('\u{258C}').unwrap()[0];
        let (right_left, _, _, _) = rects('\u{2590}').unwrap()[0];
        assert_eq!(left_right, right_left, "left and right half must meet");
        assert_eq!(area('\u{258C}') + area('\u{2590}'), 1.0);
    }

    #[test]
    fn the_eighths_step_evenly_up_to_a_full_block() {
        // U+2581..2588 grow from the bottom, one eighth at a time; U+258F..2588
        // grow from the left. An off-by-one here draws a bar chart with the wrong
        // value in it, which is exactly the sort of thing nobody notices.
        for n in 1..=8u32 {
            let lower = char::from_u32(0x2580 + n).unwrap();
            assert_eq!(area(lower), n as f32 / 8.0, "lower {n}/8");
            let left = char::from_u32(0x2590 - n).unwrap();
            assert_eq!(area(left), n as f32 / 8.0, "left {n}/8");
        }
        assert_eq!(rects('\u{2588}').unwrap(), FULL);
    }

    #[test]
    fn quadrant_characters_cover_the_right_fraction() {
        for (c, quarters) in [
            ('\u{2596}', 1),
            ('\u{2597}', 1),
            ('\u{2598}', 1),
            ('\u{259D}', 1),
            ('\u{259A}', 2),
            ('\u{259E}', 2),
            ('\u{2599}', 3),
            ('\u{259B}', 3),
            ('\u{259C}', 3),
            ('\u{259F}', 3),
        ] {
            assert_eq!(
                area(c),
                quarters as f32 / 4.0,
                "U+{:04X} should cover {quarters}/4",
                c as u32
            );
        }
    }

    #[test]
    fn no_character_overlaps_itself_or_leaves_the_cell() {
        // Areas are summed to check coverage, so overlapping rects would make that
        // arithmetic lie. Straying outside the cell would bleed into a neighbour.
        for c in drawn() {
            let rs = rects(c).unwrap();
            for (x0, y0, x1, y1) in rs {
                assert!(
                    (0.0..=1.0).contains(x0)
                        && (0.0..=1.0).contains(y0)
                        && (0.0..=1.0).contains(x1)
                        && (0.0..=1.0).contains(y1)
                        && x0 < x1
                        && y0 < y1,
                    "U+{:04X} has a bad rect {:?}",
                    c as u32,
                    (x0, y0, x1, y1)
                );
            }
            for (i, a) in rs.iter().enumerate() {
                for b in &rs[i + 1..] {
                    let overlap =
                        a.0.max(b.0) < a.2.min(b.2) && a.1.max(b.1) < a.3.min(b.3);
                    assert!(!overlap, "U+{:04X} rects overlap", c as u32);
                }
            }
        }
    }

    #[test]
    fn adjacent_cells_share_an_edge_exactly() {
        // The property that removes the seam. Cell k's right edge and cell k+1's
        // left edge are the same expression, so they can't disagree — whatever the
        // fractional advance is.
        let cw = 7.51953_f32;
        let boundary = |col: usize| (10.0 + col as f32 * cw).round();
        for col in 0..40 {
            let right = edge(boundary(col), boundary(col + 1), 1.0);
            let left = edge(boundary(col + 1), boundary(col + 2), 0.0);
            assert_eq!(right, left, "column {col} boundary");
        }
    }

    #[test]
    fn a_split_cell_meets_on_one_pixel() {
        // `▀` and `▄` in the same cell must share their inner edge, or the seam
        // reappears in the middle of the cell instead of between cells.
        let ch = 16.1_f32;
        for row in 0..20 {
            let top = (5.0 + row as f32 * ch).round();
            let bottom = (5.0 + (row + 1) as f32 * ch).round();
            let upper_bottom = edge(top, bottom, 0.5);
            let lower_top = edge(top, bottom, 0.5);
            assert_eq!(upper_bottom, lower_top);
            assert!(upper_bottom.fract() == 0.0, "must land on a whole pixel");
            assert!(upper_bottom > top && upper_bottom < bottom);
        }
    }

    #[test]
    fn full_width_characters_are_the_mergeable_ones() {
        // The optimisation only applies where a run really is one rectangle.
        assert_eq!(merges_horizontally('\u{2588}'), Some((0.0, 1.0)));
        assert_eq!(merges_horizontally('\u{2580}'), Some((0.0, 0.5)));
        assert_eq!(merges_horizontally('\u{2584}'), Some((0.5, 1.0)));
        // Partial-width and multi-rect characters don't tile into one bar.
        assert_eq!(merges_horizontally('\u{258C}'), None);
        assert_eq!(merges_horizontally('\u{2590}'), None);
        assert_eq!(merges_horizontally('\u{259A}'), None);
        assert_eq!(merges_horizontally('a'), None);
    }
}
