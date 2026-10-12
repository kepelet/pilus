//! Box drawing (U+2500–U+257F) painted from cell geometry instead of a font.
//!
//! `render_row` hands each box-drawing cell here and paints what comes back:
//! straight strokes as rects, arcs and diagonals as stroked paths. A font
//! draws these glyphs between its own ascent and descent and centers them on
//! its own metrics, not the grid's, so `│` stacked across rows leaves a gap
//! where the row is taller than the glyph, and a corner such as `┘` meets its
//! neighbors' strokes off-center. Painting every stroke from the cell's own
//! center lines makes borders tile across rows and columns alike.

use gpui::{Path, PathBuilder, Pixels, Point, point, px};

/// Arm weights for U+2500–U+257F, two bits each, packed up/right/down/left
/// from the high bits: 0 none, 1 light, 2 heavy, 3 double. Zero marks a glyph
/// that is not built from arms (dashes, arcs, diagonals). Derived from the
/// Unicode character names.
const ARMS: [u8; 128] = [
    0x11, 0x22, 0x44, 0x88, 0x00, 0x00, 0x00, 0x00, // U+2500
    0x00, 0x00, 0x00, 0x00, 0x14, 0x24, 0x18, 0x28, // U+2508
    0x05, 0x06, 0x09, 0x0a, 0x50, 0x60, 0x90, 0xa0, // U+2510
    0x41, 0x42, 0x81, 0x82, 0x54, 0x64, 0x94, 0x58, // U+2518
    0x98, 0xa4, 0x68, 0xa8, 0x45, 0x46, 0x85, 0x49, // U+2520
    0x89, 0x86, 0x4a, 0x8a, 0x15, 0x16, 0x25, 0x26, // U+2528
    0x19, 0x1a, 0x29, 0x2a, 0x51, 0x52, 0x61, 0x62, // U+2530
    0x91, 0x92, 0xa1, 0xa2, 0x55, 0x56, 0x65, 0x66, // U+2538
    0x95, 0x59, 0x99, 0x96, 0xa5, 0x5a, 0x69, 0xa6, // U+2540
    0x6a, 0x9a, 0xa9, 0xaa, 0x00, 0x00, 0x00, 0x00, // U+2548
    0x33, 0xcc, 0x34, 0x1c, 0x3c, 0x07, 0x0d, 0x0f, // U+2550
    0x70, 0xd0, 0xf0, 0x43, 0xc1, 0xc3, 0x74, 0xdc, // U+2558
    0xfc, 0x47, 0xcd, 0xcf, 0x37, 0x1d, 0x3f, 0x73, // U+2560
    0xd1, 0xf3, 0x77, 0xdd, 0xff, 0x00, 0x00, 0x00, // U+2568
    0x00, 0x00, 0x00, 0x00, 0x01, 0x40, 0x10, 0x04, // U+2570
    0x02, 0x80, 0x20, 0x08, 0x21, 0x48, 0x12, 0x84, // U+2578
];

const LIGHT: u8 = 1;
const HEAVY: u8 = 2;
const DOUBLE: u8 = 3;

/// How far a cubic's control points sit along the tangent for a quarter circle.
const QUARTER_CIRCLE: f32 = 0.552_284_8;

/// The box-drawing character a cell holds, if that is all it holds.
pub(super) fn box_char(symbol: &str) -> Option<char> {
    let mut chars = symbol.chars();
    let c = chars.next()?;
    (chars.next().is_none() && matches!(c, '\u{2500}'..='\u{257f}')).then_some(c)
}

/// Which sides of its cell a solid box-drawing glyph reaches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Arms {
    pub up: bool,
    pub right: bool,
    pub down: bool,
    pub left: bool,
}

/// The sides `c` connects to, or `None` for a glyph that is not built from
/// arms (dashes, diagonals) and so cannot continue a line. Weight is ignored:
/// a heavy or double stroke reaches the same sides as a light one.
pub(super) fn arms(c: char) -> Option<Arms> {
    let index = (c as u32).checked_sub(0x2500).filter(|i| *i < 0x80)?;
    let packed = match c {
        // Arcs are not in `ARMS`; they connect like the square corners.
        '\u{256d}' => 0x14,
        '\u{256e}' => 0x05,
        '\u{256f}' => 0x41,
        '\u{2570}' => 0x50,
        _ => ARMS[index as usize],
    };
    (packed != 0).then(|| {
        let reaches = |shift: u8| (packed >> shift) & 0b11 != 0;
        Arms {
            up: reaches(6),
            right: reaches(4),
            down: reaches(2),
            left: reaches(0),
        }
    })
}

/// The light stroke width for a terminal font size; heavy is twice this and a
/// double line is two light strokes a light width apart. Whole pixels keep the
/// edges crisp, and the width grows with the font as the glyph strokes would.
pub(super) fn light_stroke(font_size: f32) -> f32 {
    (font_size / 13.0).round().max(1.0)
}

/// An axis-aligned rect in row-local pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

/// A stroke that is not axis-aligned, in row-local pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Curve {
    Line {
        from: [f32; 2],
        to: [f32; 2],
    },
    /// A straight run into a quarter-circle bend and out along the other axis.
    Arc {
        start: [f32; 2],
        bend_from: [f32; 2],
        control_a: [f32; 2],
        control_b: [f32; 2],
        bend_to: [f32; 2],
        end: [f32; 2],
    },
}

impl Curve {
    /// The stroked path, offset to the row's window `origin`.
    pub(super) fn path(&self, origin: Point<Pixels>, width: f32) -> Option<Path<Pixels>> {
        let at = |[x, y]: [f32; 2]| point(origin.x + px(x), origin.y + px(y));
        let mut builder = PathBuilder::stroke(px(width));
        match *self {
            Curve::Line { from, to } => {
                builder.move_to(at(from));
                builder.line_to(at(to));
            }
            Curve::Arc {
                start,
                bend_from,
                control_a,
                control_b,
                bend_to,
                end,
            } => {
                builder.move_to(at(start));
                builder.line_to(at(bend_from));
                builder.cubic_bezier_to(at(bend_to), at(control_a), at(control_b));
                builder.line_to(at(end));
            }
        }
        builder.build().ok()
    }
}

/// One box-drawing glyph's strokes.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Strokes {
    pub rects: Vec<Rect>,
    pub curves: Vec<Curve>,
}

/// The strokes for `c` in the cell spanning `left..right` and `0..height`,
/// or `None` for a character outside U+2500–U+257F.
///
/// `left` and `right` should already sit on whole pixels, so a stroke that
/// runs to the cell edge meets its neighbor's on exactly the same coordinate.
pub(super) fn strokes(c: char, left: f32, right: f32, height: f32, light: f32) -> Option<Strokes> {
    let index = (c as u32).checked_sub(0x2500)?;
    if index >= 0x80 {
        return None;
    }
    let t = light;
    // The light stroke's edges land on whole pixels; everything else is
    // measured from its center line so all weights share one axis.
    let vx = ((left + right) / 2.0 - t / 2.0).round() + t / 2.0;
    let hy = (height / 2.0 - t / 2.0).round() + t / 2.0;
    let cell = Cell {
        left,
        right,
        height,
        vx,
        hy,
        t,
    };
    let mut strokes = Strokes::default();
    match c {
        '\u{2504}'..='\u{250b}' | '\u{254c}'..='\u{254f}' => cell.dashes(c, &mut strokes),
        '\u{256d}'..='\u{2570}' => strokes.curves.push(cell.arc(c)),
        '\u{2571}' => strokes.curves.push(cell.diagonal(true)),
        '\u{2572}' => strokes.curves.push(cell.diagonal(false)),
        '\u{2573}' => {
            strokes.curves.push(cell.diagonal(true));
            strokes.curves.push(cell.diagonal(false));
        }
        _ => {
            let packed = ARMS[index as usize];
            let arm = |shift: u8| (packed >> shift) & 0b11;
            cell.arms([arm(6), arm(4), arm(2), arm(0)], &mut strokes);
        }
    }
    Some(strokes)
}

/// One cell's frame: its edges and the center lines every stroke runs along.
struct Cell {
    left: f32,
    right: f32,
    height: f32,
    vx: f32,
    hy: f32,
    t: f32,
}

impl Cell {
    fn half(&self, weight: u8) -> f32 {
        match weight {
            0 => 0.0,
            LIGHT => self.t / 2.0,
            HEAVY => self.t,
            _ => 1.5 * self.t,
        }
    }

    /// Paint each arm from its cell edge toward the center, stopping where it
    /// meets the perpendicular strokes.
    fn arms(&self, [up, right, down, left]: [u8; 4], strokes: &mut Strokes) {
        let mut push = |rect: Rect| {
            let rect = Rect {
                x0: rect.x0.round(),
                y0: rect.y0.round(),
                x1: rect.x1.round(),
                y1: rect.y1.round(),
            };
            if rect.x1 > rect.x0 && rect.y1 > rect.y0 {
                strokes.rects.push(rect);
            }
        };
        // Vertical arms: perpendiculars are left (before) and right (after).
        for (weight, opposite, toward_top) in [(up, down, true), (down, up, false)] {
            for (offset, width, reach) in self.arm_strokes(weight, opposite, left, right) {
                let x = self.vx + offset;
                let (y0, y1) = if toward_top {
                    (0.0, self.hy + reach)
                } else {
                    (self.hy - reach, self.height)
                };
                push(Rect {
                    x0: x - width / 2.0,
                    y0,
                    x1: x + width / 2.0,
                    y1,
                });
            }
        }
        // Horizontal arms: perpendiculars are up (before) and down (after).
        for (weight, opposite, toward_left) in [(left, right, true), (right, left, false)] {
            for (offset, width, reach) in self.arm_strokes(weight, opposite, up, down) {
                let y = self.hy + offset;
                let (x0, x1) = if toward_left {
                    (self.left, self.vx + reach)
                } else {
                    (self.vx - reach, self.right)
                };
                push(Rect {
                    x0,
                    y0: y - width / 2.0,
                    x1,
                    y1: y + width / 2.0,
                });
            }
        }
    }

    /// `(offset from the center line, width, reach past the center)` for each
    /// stroke of one arm. `before` and `after` are the perpendicular arms on
    /// the side of negative and positive offsets.
    fn arm_strokes(&self, weight: u8, opposite: u8, before: u8, after: u8) -> Vec<(f32, f32, f32)> {
        let t = self.t;
        match weight {
            0 => Vec::new(),
            DOUBLE => [(-1.0, before, after), (1.0, after, before)]
                .into_iter()
                .map(|(side, own, other)| {
                    // The inner stroke of a corner stops at the near stroke
                    // of a double perpendicular; the outer one runs to its
                    // far stroke so the two lines nest.
                    let reach = if own == DOUBLE {
                        -0.5 * t
                    } else if own > 0 {
                        self.half(own)
                    } else if other == DOUBLE {
                        1.5 * t
                    } else {
                        self.half(other)
                    };
                    (side * t, t, reach)
                })
                .collect(),
            _ => {
                let reach = if opposite > 0 {
                    0.0
                } else if before == DOUBLE && after == DOUBLE {
                    // A tee into a double line touches only its near stroke.
                    -0.5 * t
                } else {
                    self.half(before).max(self.half(after))
                };
                let width = if weight == HEAVY { 2.0 * t } else { t };
                vec![(0.0, width, reach)]
            }
        }
    }

    fn dashes(&self, c: char, strokes: &mut Strokes) {
        let (count, first) = match c {
            '\u{2504}'..='\u{2507}' => (3u8, 0x2504),
            '\u{2508}'..='\u{250b}' => (4, 0x2508),
            _ => (2, 0x254c),
        };
        let variant = c as u32 - first;
        let width = if variant & 1 == 1 {
            2.0 * self.t
        } else {
            self.t
        };
        let vertical = variant >= 2;
        let (start, end) = if vertical {
            (0.0, self.height)
        } else {
            (self.left, self.right)
        };
        let segment = (end - start) / f32::from(count);
        let gap = segment * 0.4;
        for dash in 0..count {
            let a = (start + f32::from(dash) * segment + gap / 2.0).round();
            let b = (start + f32::from(dash + 1) * segment - gap / 2.0).round();
            if b <= a {
                continue;
            }
            strokes.rects.push(if vertical {
                Rect {
                    x0: (self.vx - width / 2.0).round(),
                    y0: a,
                    x1: (self.vx + width / 2.0).round(),
                    y1: b,
                }
            } else {
                Rect {
                    x0: a,
                    y0: (self.hy - width / 2.0).round(),
                    x1: b,
                    y1: (self.hy + width / 2.0).round(),
                }
            });
        }
    }

    /// `╭╮╯╰`: the straight arms of the matching corner joined by a quarter
    /// circle as wide as the cell allows.
    fn arc(&self, c: char) -> Curve {
        let (dx, dy) = match c {
            '\u{256d}' => (1.0, 1.0),
            '\u{256e}' => (-1.0, 1.0),
            '\u{256f}' => (-1.0, -1.0),
            _ => (1.0, -1.0),
        };
        let (vx, hy) = (self.vx, self.hy);
        let radius = (self.right - vx)
            .min(vx - self.left)
            .min(hy)
            .min(self.height - hy)
            .max(0.0);
        let pull = radius * (1.0 - QUARTER_CIRCLE);
        Curve::Arc {
            start: [vx, if dy > 0.0 { self.height } else { 0.0 }],
            bend_from: [vx, hy + dy * radius],
            control_a: [vx, hy + dy * pull],
            control_b: [vx + dx * pull, hy],
            bend_to: [vx + dx * radius, hy],
            end: [if dx > 0.0 { self.right } else { self.left }, hy],
        }
    }

    /// `╱` (`rising`) or `╲`, corner to corner so neighbors continue it.
    fn diagonal(&self, rising: bool) -> Curve {
        let (top, bottom) = (0.0, self.height);
        if rising {
            Curve::Line {
                from: [self.left, bottom],
                to: [self.right, top],
            }
        } else {
            Curve::Line {
                from: [self.left, top],
                to: [self.right, bottom],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Menlo 13 cell: 8px wide (already snapped), 17px tall, 1px light.
    fn rects(c: char) -> Vec<Rect> {
        strokes(c, 16.0, 24.0, 17.0, 1.0).expect("box").rects
    }

    #[test]
    fn vertical_line_spans_the_whole_row() {
        // The font's `│` stops at its descent and leaves a gap between rows.
        assert_eq!(
            rects('│'),
            vec![
                Rect {
                    x0: 20.0,
                    y0: 0.0,
                    x1: 21.0,
                    y1: 8.5_f32.round()
                },
                Rect {
                    x0: 20.0,
                    y0: 8.5_f32.round(),
                    x1: 21.0,
                    y1: 17.0
                },
            ]
        );
    }

    #[test]
    fn horizontal_line_spans_the_whole_cell() {
        let rects = rects('─');
        assert_eq!(rects.first().map(|r| r.x0), Some(16.0));
        assert_eq!(rects.last().map(|r| r.x1), Some(24.0));
        assert!(rects.iter().all(|r| r.y0 == 8.0 && r.y1 == 9.0));
    }

    #[test]
    fn corners_meet_on_the_shared_center_lines() {
        // `┘` and `└` side by side: each vertical arm runs from the row top
        // down through the horizontal stroke, and the horizontals sit on the
        // same row as `─` so the corner meets its border.
        let up_left = rects('┘');
        let up_right = rects('└');
        let horizontal = rects('─');
        for corner in [&up_left, &up_right] {
            let vertical = corner.iter().find(|r| r.y0 == 0.0).expect("up arm");
            assert_eq!((vertical.x0, vertical.x1), (20.0, 21.0));
            assert_eq!(vertical.y1, 9.0);
            let across = corner.iter().find(|r| r.y0 != 0.0).expect("side arm");
            assert_eq!((across.y0, across.y1), (horizontal[0].y0, horizontal[0].y1));
        }
        assert_eq!(up_left.iter().map(|r| r.x0).fold(f32::MAX, f32::min), 16.0);
        assert_eq!(up_right.iter().map(|r| r.x1).fold(0.0, f32::max), 24.0);
    }

    #[test]
    fn double_corner_nests_its_lines() {
        // `╔`: the outer strokes meet at the top-left, the inner strokes one
        // gap in, and neither crosses the other's line.
        let rects = rects('╔');
        assert_eq!(rects.len(), 4);
        let outer_down = rects.iter().find(|r| r.x0 == 19.0).expect("outer");
        let inner_down = rects.iter().find(|r| r.x0 == 21.0).expect("inner");
        assert_eq!(outer_down.y0, 7.0);
        assert_eq!(inner_down.y0, 9.0);
    }

    #[test]
    fn arcs_and_diagonals_draw_as_curves() {
        let arc = strokes('╭', 16.0, 24.0, 17.0, 1.0).expect("box");
        assert!(arc.rects.is_empty());
        match arc.curves.as_slice() {
            [Curve::Arc { start, end, .. }] => {
                assert_eq!(*start, [20.5, 17.0]);
                assert_eq!(*end, [24.0, 8.5]);
            }
            other => panic!("unexpected {other:?}"),
        }
        let cross = strokes('╳', 16.0, 24.0, 17.0, 1.0).expect("box");
        assert_eq!(cross.curves.len(), 2);
    }

    #[test]
    fn dashes_leave_gaps() {
        assert_eq!(rects('┄').len(), 3);
        assert_eq!(rects('┊').len(), 4);
        assert_eq!(rects('╌').len(), 2);
    }

    #[test]
    fn arms_name_the_sides_a_glyph_reaches() {
        let arms = |c| arms(c).map(|a| (a.up, a.right, a.down, a.left));
        assert_eq!(arms('\u{2502}'), Some((true, false, true, false)));
        assert_eq!(arms('\u{251c}'), Some((true, true, true, false)));
        assert_eq!(arms('\u{2550}'), Some((false, true, false, true)));
        assert_eq!(arms('\u{256d}'), Some((false, true, true, false)));
        assert_eq!(arms('\u{256f}'), Some((true, false, false, true)));
        assert_eq!(arms('\u{2504}'), None);
        assert_eq!(arms('x'), None);
    }

    #[test]
    fn only_box_drawing_is_claimed() {
        assert_eq!(box_char("│"), Some('│'));
        assert_eq!(box_char("▀"), None);
        assert_eq!(box_char("a"), None);
        assert_eq!(box_char("││"), None);
        assert!(strokes('a', 0.0, 8.0, 17.0, 1.0).is_none());
        for c in '\u{2500}'..='\u{257f}' {
            let painted = strokes(c, 16.0, 24.0, 17.0, 1.0).expect("box");
            assert!(
                !painted.rects.is_empty() || !painted.curves.is_empty(),
                "{c} paints nothing"
            );
        }
    }
}
