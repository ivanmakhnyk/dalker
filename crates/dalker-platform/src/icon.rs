//! The app's icon: the Dalker mark (a "D" whose counter holds a ball, joined by a smooth neck
//! to a second ball outside the letter; see `assets/branding/dalker-mark.svg`) with a dark
//! outline, transparent everywhere else. Drawn here so the tray icon and the settings window need no image files;
//! `examples/make_icons.rs` uses the same drawing for the app icons in `assets/branding/`.

/// Side of [`rgba`]'s square, in pixels.
pub const SIZE: usize = 32;

/// The outline around the mark, which also fills the D's counter and the gap around the balls.
pub const DARK: (u8, u8, u8) = (0x1D, 0x1F, 0x24);
/// The mark's orange, from the top left to the bottom right of the mark.
pub const ORANGE: [(u8, u8, u8); 2] = [(0xF6, 0x87, 0x3A), (0xE2, 0x56, 0x1F)];
/// The mark's colour in the tray icon while somebody is connected.
pub const RED: (u8, u8, u8) = (0xE0, 0x31, 0x31);

/// How the icon is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// The orange mark: the app icon and the idle tray icon.
    Normal,
    /// The mark in red: the tray icon while somebody is connected.
    Connected,
}

type Pt = (f32, f32);

// The mark's geometry, in the units of `assets/branding/dalker-mark.svg`.

/// The mark's bounding box: x, y, w, h.
const MARK_BOX: (f32, f32, f32, f32) = (29.0, 52.0, 161.0, 156.0);
/// How far the dark outline reaches beyond the mark.
const OUTLINE: f32 = 10.0;
/// The box [`rgba_sized`] fits into the icon: the mark box grown by a bit more than the outline.
const ICON_BOX: (f32, f32, f32, f32) = (18.0, 41.0, 183.0, 178.0);
/// Centres of the ball inside the D and of the ball outside it, and their radius.
const BALL_IN: Pt = (115.0, 120.0);
const BALL_OUT: Pt = (53.0, 184.0);
const BALL_R: f32 = 24.0;
/// Width of the neck between the balls at its narrowest, and the radius of the concave arcs
/// that blend it into the balls.
const NECK: f32 = 16.0;
const FILLET: f32 = 40.0;
/// The gap kept between the balls and neck and the D around them.
const GAP: f32 = 10.0;
/// The D is a stroke of this width along a stem at x = 60, bars at y = 64 and 176 and a
/// half-circle bowl around (122, 120).
const STROKE: f32 = 24.0;
const D_STEM_X: f32 = 60.0;
const D_TOP: f32 = 64.0;
const D_BOTTOM: f32 = 176.0;
const BOWL: Pt = (122.0, 120.0);
const BOWL_R: f32 = 56.0;

fn dist(a: Pt, b: Pt) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

fn segment_dist(p: Pt, a: Pt, b: Pt) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    dist(p, (a.0 + t * dx, a.1 + t * dy))
}

/// Inside the two balls or the neck between them, all grown outwards by `grow`.
fn in_balls(p: Pt, grow: f32) -> bool {
    if dist(p, BALL_IN) <= BALL_R + grow || dist(p, BALL_OUT) <= BALL_R + grow {
        return true;
    }
    // Along the axis between the centres (t) and away from it (s).
    let len = dist(BALL_IN, BALL_OUT);
    let u = ((BALL_OUT.0 - BALL_IN.0) / len, (BALL_OUT.1 - BALL_IN.1) / len);
    let (dx, dy) = (p.0 - BALL_IN.0, p.1 - BALL_IN.1);
    let t = dx * u.0 + dy * u.1;
    let s = (dy * u.0 - dx * u.1).abs();
    // The neck is symmetric: measure t from the nearer centre.
    let t = t.min(len - t);
    // The fillet circles sit at t_fillet, touching a ball and the neck's straight edge; the
    // neck starts where they touch the ball. Growing the shape shrinks the concave arcs.
    let t_fillet = ((BALL_R + FILLET).powi(2) - (NECK / 2.0 + FILLET).powi(2)).sqrt();
    let t_start = t_fillet * (BALL_R + grow) / (BALL_R + FILLET);
    if t < t_start {
        return false;
    }
    let (half, arc) = (NECK / 2.0 + grow, FILLET - grow);
    let edge = if t >= t_fillet { half } else { half + arc - (arc * arc - (t - t_fillet).powi(2)).max(0.0).sqrt() };
    s <= edge
}

/// On the D's stroke, grown outwards on both sides by `grow`.
fn in_d(p: Pt, grow: f32) -> bool {
    let h = STROKE / 2.0 + grow;
    let (top_left, bottom_left) = ((D_STEM_X, D_TOP), (D_STEM_X, D_BOTTOM));
    segment_dist(p, bottom_left, top_left) <= h
        || segment_dist(p, top_left, (BOWL.0, D_TOP)) <= h
        || segment_dist(p, (BOWL.0, D_BOTTOM), bottom_left) <= h
        || (p.0 >= BOWL.0 && (dist(p, BOWL) - BOWL_R).abs() <= h)
}

/// Whether a point (in the mark's units) is inside the mark.
fn in_mark(p: Pt) -> bool {
    in_balls(p, 0.0) || (in_d(p, 0.0) && !in_balls(p, GAP))
}

/// Whether a point is inside the dark outline: the D (counter included) and the balls and neck,
/// all grown by [`OUTLINE`]. The gap between them is not cut out of it, so it shows dark.
fn in_outline(p: Pt) -> bool {
    let in_d_area = p.0 >= D_STEM_X
        && p.1 >= D_TOP
        && p.1 <= D_BOTTOM
        && (p.0 <= BOWL.0 || dist(p, BOWL) <= BOWL_R);
    in_d_area || in_d(p, OUTLINE) || in_balls(p, OUTLINE)
}

/// The mark's orange at a point: a gradient from the top left to the bottom right.
fn orange(p: Pt) -> (f32, f32, f32) {
    let (a, b) = ((30.0, 40.0), (190.0, 210.0));
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let [c0, c1] = ORANGE;
    let mix = |x: u8, y: u8| x as f32 + (y as f32 - x as f32) * t;
    (mix(c0.0, c1.0), mix(c0.1, c1.1), mix(c0.2, c1.2))
}

/// 32x32 RGBA (straight alpha): the outlined mark, coloured as `look` says.
pub fn rgba(look: Look) -> Vec<u8> {
    rgba_sized(look, SIZE)
}

/// Where [`rgba_sized`] puts the mark: its scale (pixels per mark unit) and the pixel
/// position of the mark box's top left corner. [`ICON_BOX`] fills the icon's width and is
/// centred vertically.
fn placement(size: usize) -> (f32, f32, f32) {
    let n = size as f32;
    let scale = n / ICON_BOX.2;
    let (x, y) = ((n - ICON_BOX.2 * scale) / 2.0, (n - ICON_BOX.3 * scale) / 2.0);
    (scale, x + (MARK_BOX.0 - ICON_BOX.0) * scale, y + (MARK_BOX.1 - ICON_BOX.1) * scale)
}

/// [`rgba`] at `size` x `size` pixels.
pub fn rgba_sized(look: Look, size: usize) -> Vec<u8> {
    let (scale, ox, oy) = placement(size);
    let to_mark = |x: f32, y: f32| -> Pt { (MARK_BOX.0 + (x - ox) / scale, MARK_BOX.1 + (y - oy) / scale) };
    let mut px = vec![0u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            // 4x4 supersampling for smooth edges.
            let (mut hits, mut sum) = (0u32, (0.0f32, 0.0f32, 0.0f32));
            for sy in 0..4 {
                for sx in 0..4 {
                    let (fx, fy) = (x as f32 + (sx as f32 + 0.5) / 4.0, y as f32 + (sy as f32 + 0.5) / 4.0);
                    let p = to_mark(fx, fy);
                    if !in_outline(p) {
                        continue;
                    }
                    hits += 1;
                    let c = match (in_mark(p), look) {
                        (false, _) => (DARK.0 as f32, DARK.1 as f32, DARK.2 as f32),
                        (true, Look::Normal) => orange(p),
                        (true, Look::Connected) => (RED.0 as f32, RED.1 as f32, RED.2 as f32),
                    };
                    sum = (sum.0 + c.0, sum.1 + c.1, sum.2 + c.2);
                }
            }
            if hits == 0 {
                continue;
            }
            let i = (y * size + x) * 4;
            let k = hits as f32;
            px[i] = (sum.0 / k).round() as u8;
            px[i + 1] = (sum.1 / k).round() as u8;
            px[i + 2] = (sum.2 / k).round() as u8;
            px[i + 3] = (hits * 255 / 16) as u8;
        }
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_has_both_balls_and_a_gap_around_them() {
        let size = 128;
        let px = rgba_sized(Look::Normal, size);
        let (scale, ox, oy) = placement(size);
        let at = |m: Pt| {
            // A point in the mark's units, as rgba_sized places it.
            let x = ox + (m.0 - MARK_BOX.0) * scale;
            let y = oy + (m.1 - MARK_BOX.1) * scale;
            let i = (y as usize * size + x as usize) * 4;
            (px[i], px[i + 1], px[i + 2], px[i + 3])
        };
        let is_orange = |c: (u8, u8, u8, u8)| c.0 > 200 && c.1 > 70 && c.1 < 150 && c.2 < 80 && c.3 == 255;
        let dark = (DARK.0, DARK.1, DARK.2, 255);
        assert!(is_orange(at(BALL_IN)), "ball inside the D: {:?}", at(BALL_IN));
        assert!(is_orange(at(BALL_OUT)), "ball outside the D: {:?}", at(BALL_OUT));
        assert!(is_orange(at((84.0, 152.0))), "the neck between the balls");
        assert!(is_orange(at((D_STEM_X, 80.0))), "D stem");
        assert!(is_orange(at((BOWL.0 + BOWL_R, BOWL.1))), "D bowl");
        assert_eq!(at((152.0, 120.0)), dark, "D counter, beside the inner ball");
        assert_eq!(at((D_STEM_X, BALL_OUT.1 - BALL_R - 5.0)), dark, "gap between the outer ball and the stem");
        assert_eq!(at((D_STEM_X - STROKE / 2.0 - OUTLINE / 2.0, 120.0)), dark, "outline left of the stem");
        assert_eq!(px[3], 0, "corners are transparent");
        assert_eq!(at((185.0, 57.0)).3, 0, "outside the outline, above the bowl, is transparent");
        assert_eq!(at((30.0, 100.0)).3, 0, "outside the outline, left of the stem, is transparent");
    }

    #[test]
    fn connected_icon_is_red_with_the_dark_outline() {
        let size = 64;
        let px = rgba_sized(Look::Connected, size);
        let (scale, ox, oy) = placement(size);
        let at = |m: Pt| {
            let x = ox + (m.0 - MARK_BOX.0) * scale;
            let y = oy + (m.1 - MARK_BOX.1) * scale;
            let i = (y as usize * size + x as usize) * 4;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        assert_eq!(at(BALL_IN), [RED.0, RED.1, RED.2, 255]);
        assert_eq!(at((152.0, 120.0)), [DARK.0, DARK.1, DARK.2, 255], "D counter");
        assert_eq!(px[3], 0, "corners are transparent");
    }
}
