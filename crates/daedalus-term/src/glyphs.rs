//! Box-drawing lines and block elements drawn as quads, so they meet across the 1.4× line height
//! that font glyphs leave gaps in.

use gpui::{Bounds, Hsla, PaintQuad, Pixels, fill, point, px, size};

/// Weights of the up, right, down and left arms of U+2500..=U+257F (0 none, 1 light, 2 heavy).
fn arms(c: char) -> Option<[u8; 4]> {
    const LINES: [[u8; 4]; 76] = [
        [0, 1, 0, 1], // ─ 2500
        [0, 2, 0, 2],
        [1, 0, 1, 0],
        [2, 0, 2, 0],
        [0; 4], // 2504..=250B dashed lines stay with the font
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0; 4],
        [0, 1, 1, 0], // ┌ 250C
        [0, 2, 1, 0],
        [0, 1, 2, 0],
        [0, 2, 2, 0],
        [0, 0, 1, 1], // ┐ 2510
        [0, 0, 1, 2],
        [0, 0, 2, 1],
        [0, 0, 2, 2],
        [1, 1, 0, 0], // └ 2514
        [1, 2, 0, 0],
        [2, 1, 0, 0],
        [2, 2, 0, 0],
        [1, 0, 0, 1], // ┘ 2518
        [1, 0, 0, 2],
        [2, 0, 0, 1],
        [2, 0, 0, 2],
        [1, 1, 1, 0], // ├ 251C
        [1, 2, 1, 0],
        [2, 1, 1, 0],
        [1, 1, 2, 0],
        [2, 1, 2, 0],
        [2, 2, 1, 0],
        [1, 2, 2, 0],
        [2, 2, 2, 0],
        [1, 0, 1, 1], // ┤ 2524
        [1, 0, 1, 2],
        [2, 0, 1, 1],
        [1, 0, 2, 1],
        [2, 0, 2, 1],
        [2, 0, 1, 2],
        [1, 0, 2, 2],
        [2, 0, 2, 2],
        [0, 1, 1, 1], // ┬ 252C
        [0, 1, 1, 2],
        [0, 2, 1, 1],
        [0, 2, 1, 2],
        [0, 1, 2, 1],
        [0, 1, 2, 2],
        [0, 2, 2, 1],
        [0, 2, 2, 2],
        [1, 1, 0, 1], // ┴ 2534
        [1, 1, 0, 2],
        [1, 2, 0, 1],
        [1, 2, 0, 2],
        [2, 1, 0, 1],
        [2, 1, 0, 2],
        [2, 2, 0, 1],
        [2, 2, 0, 2],
        [1, 1, 1, 1], // ┼ 253C
        [1, 1, 1, 2],
        [1, 2, 1, 1],
        [1, 2, 1, 2],
        [2, 1, 1, 1],
        [1, 1, 2, 1],
        [2, 1, 2, 1],
        [2, 1, 1, 2],
        [2, 2, 1, 1],
        [1, 1, 2, 2],
        [1, 2, 2, 1],
        [2, 2, 1, 2],
        [1, 2, 2, 2],
        [2, 1, 2, 2],
        [2, 2, 2, 1],
        [2, 2, 2, 2], // ╋ 254B
    ];
    let arms = match c as u32 {
        i @ 0x2500..=0x254B => LINES[(i - 0x2500) as usize],
        // Rounded corners are drawn square; the arms still meet their neighbours.
        0x256D => [0, 1, 1, 0],
        0x256E => [0, 0, 1, 1],
        0x256F => [1, 0, 0, 1],
        0x2570 => [1, 1, 0, 0],
        0x2574 => [0, 0, 0, 1],
        0x2575 => [1, 0, 0, 0],
        0x2576 => [0, 1, 0, 0],
        0x2577 => [0, 0, 1, 0],
        0x2578 => [0, 0, 0, 2],
        0x2579 => [2, 0, 0, 0],
        0x257A => [0, 2, 0, 0],
        0x257B => [0, 0, 2, 0],
        0x257C => [0, 2, 0, 1],
        0x257D => [1, 0, 2, 0],
        0x257E => [0, 1, 0, 2],
        0x257F => [2, 0, 1, 0],
        _ => return None,
    };
    (arms != [0; 4]).then_some(arms)
}

/// Fractions (x0, y0, x1, y1) of the cell that a block element U+2580..=U+259F covers, and its
/// opacity (below 1 for the shades).
fn block(c: char) -> Option<(Vec<[f32; 4]>, f32)> {
    const UL: [f32; 4] = [0., 0., 0.5, 0.5];
    const UR: [f32; 4] = [0.5, 0., 1., 0.5];
    const LL: [f32; 4] = [0., 0.5, 0.5, 1.];
    const LR: [f32; 4] = [0.5, 0.5, 1., 1.];
    let rects = match c as u32 {
        0x2580 => vec![[0., 0., 1., 0.5]],
        i @ 0x2581..=0x2588 => vec![[0., 1. - (i - 0x2580) as f32 / 8., 1., 1.]],
        i @ 0x2589..=0x258F => vec![[0., 0., (0x2590 - i) as f32 / 8., 1.]],
        0x2590 => vec![[0.5, 0., 1., 1.]],
        0x2591 => return Some((vec![[0., 0., 1., 1.]], 0.25)),
        0x2592 => return Some((vec![[0., 0., 1., 1.]], 0.5)),
        0x2593 => return Some((vec![[0., 0., 1., 1.]], 0.75)),
        0x2594 => vec![[0., 0., 1., 0.125]],
        0x2595 => vec![[0.875, 0., 1., 1.]],
        0x2596 => vec![LL],
        0x2597 => vec![LR],
        0x2598 => vec![UL],
        0x2599 => vec![UL, LL, LR],
        0x259A => vec![UL, LR],
        0x259B => vec![UL, UR, LL],
        0x259C => vec![UL, UR, LR],
        0x259D => vec![UR],
        0x259E => vec![UR, LL],
        0x259F => vec![UR, LL, LR],
        _ => return None,
    };
    Some((rects, 1.))
}

/// The quads that draw `c` in `cell`, or `None` if the font should draw it.
pub(crate) fn quads(c: char, cell: Bounds<Pixels>, color: Hsla) -> Option<Vec<PaintQuad>> {
    let (x, y) = (f32::from(cell.origin.x), f32::from(cell.origin.y));
    let (w, h) = (f32::from(cell.size.width), f32::from(cell.size.height));
    let rect = |x0: f32, y0: f32, x1: f32, y1: f32, color: Hsla| {
        fill(
            Bounds::new(point(px(x0), px(y0)), size(px(x1 - x0), px(y1 - y0))),
            color,
        )
    };
    if let Some((rects, alpha)) = block(c) {
        let color = color.opacity(alpha);
        return Some(
            rects
                .into_iter()
                .map(|[x0, y0, x1, y1]| rect(x + x0 * w, y + y0 * h, x + x1 * w, y + y1 * h, color))
                .collect(),
        );
    }
    let [up, right, down, left] = arms(c)?;
    let thickness = |weight: u8| weight as f32;
    let vertical = thickness(up.max(down));
    let horizontal = thickness(left.max(right));
    // Each arm runs from the cell edge through the crossing line, so joints have no notch.
    let cross_x = x + ((w - vertical.max(1.)) / 2.).floor();
    let cross_y = y + ((h - horizontal.max(1.)) / 2.).floor();
    let mut out = Vec::new();
    for (weight, is_vertical, toward_start) in [
        (up, true, true),
        (down, true, false),
        (left, false, true),
        (right, false, false),
    ] {
        if weight == 0 {
            continue;
        }
        let t = thickness(weight);
        out.push(if is_vertical {
            let x0 = x + ((w - t) / 2.).floor();
            let (y0, y1) = if toward_start {
                (y, cross_y + horizontal.max(t))
            } else {
                (cross_y, y + h)
            };
            rect(x0, y0, x0 + t, y1, color)
        } else {
            let y0 = y + ((h - t) / 2.).floor();
            let (x0, x1) = if toward_start {
                (x, cross_x + vertical.max(t))
            } else {
                (cross_x, x + w)
            };
            rect(x0, y0, x1, y0 + t, color)
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_table_matches_the_named_corners_and_crosses() {
        assert_eq!(arms('─'), Some([0, 1, 0, 1]));
        assert_eq!(arms('┏'), Some([0, 2, 2, 0]));
        assert_eq!(arms('┘'), Some([1, 0, 0, 1]));
        assert_eq!(arms('┼'), Some([1, 1, 1, 1]));
        assert_eq!(arms('╋'), Some([2, 2, 2, 2]));
        assert_eq!(arms('╭'), arms('┌'));
        assert_eq!(arms('┄'), None, "dashed lines are left to the font");
        assert_eq!(arms('a'), None);
    }

    #[test]
    fn vertical_lines_span_the_whole_row() {
        let cell = Bounds::new(point(px(16.), px(36.)), size(px(8.), px(18.)));
        let arms = quads('│', cell, gpui::black()).unwrap();
        let (top, bottom) = (arms[0].bounds.top(), arms[1].bounds.bottom());
        assert_eq!((top, bottom), (px(36.), px(54.)));
        assert!(
            arms[0].bounds.bottom() >= arms[1].bounds.top(),
            "arms overlap at the centre"
        );
        let full = quads('█', cell, gpui::black()).unwrap()[0].bounds;
        assert_eq!(full, cell);
    }
}
