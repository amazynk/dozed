//! The tray snowflake, drawn rather than looked up.
//!
//! ## Why not an icon name
//!
//! The first version named themed icons (`dozed`, `dozed-holding`) and shipped
//! SVGs into `hicolor`, on the reasoning that a name lets the panel scale and
//! theme them. The names resolve — `Gtk.IconTheme` finds all three — and the
//! tray was still empty, because resolving a name is only as good as the host's
//! icon cache *at the moment it looks up*. A panel already running when the
//! installer wrote the files may never see them, and a name that fails to
//! resolve is not a fallback, it is a blank space where the icon should be.
//!
//! Sending pixels removes the entire class of problem: the bytes travel in the
//! D-Bus message, so there is nothing to find, nothing to cache, and nothing to
//! invalidate. It is the pattern ksni's own custom-icon example uses.
//!
//! ## Why drawn rather than embedded
//!
//! The usual way to send pixels is to embed a PNG and decode it, which costs an
//! image-decoding dependency and pins the icon to whatever sizes somebody
//! remembered to export. The flake is three crossed lines and six chevrons, so
//! it is cheaper to describe than to store: the geometry below is rasterised at
//! whatever size the panel asks for. No blobs in the repository, no new
//! dependency, and a high-DPI panel gets a sharp icon rather than a scaled
//! 22-pixel one.
//!
//! `packaging/icons/*.svg` is the same drawing in the same sixteen-unit space,
//! and is **not** what the tray shows — it is the application icon, for the
//! window and for a desktop entry. Changing one does not change the other, which
//! is worth knowing before editing either.
//!
//! Anti-aliased by distance rather than by supersampling: every shape here is a
//! thick line segment or a disc, and for both the distance from a pixel centre
//! to the shape is a closed form. Coverage is that distance across one pixel.

use ksni::Icon;

/// The design space the geometry is written in, matching the SVGs.
const SPAN: f32 = 16.0;

/// One arm of the flake and its two chevrons, before rotation.
///
/// The main stroke runs the full diameter, so three rotations produce six arms.
const ARM: &[Segment] = &[
    Segment { x0: 8.0, y0: 1.7, x1: 8.0, y1: 14.3 },
    Segment { x0: 6.2, y0: 3.4, x1: 8.0, y1: 5.2 },
    Segment { x0: 8.0, y0: 5.2, x1: 9.8, y1: 3.4 },
    Segment { x0: 6.2, y0: 12.6, x1: 8.0, y1: 10.8 },
    Segment { x0: 8.0, y0: 10.8, x1: 9.8, y1: 12.6 },
];

/// Which of the three the panel should be showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Watching, but holding nothing: a thin outline.
    Idle,
    /// Holding something: heavier, with a solid hub.
    Holding,
    /// Snoozed: grey, with a pause badge.
    ///
    /// Two signals rather than one, because this is the state a user is most
    /// likely to be in without remembering it, and at 22 pixels a change of
    /// colour alone is not reliably noticed.
    Snoozed,
}

/// The sizes offered to the panel.
///
/// A host picks the nearest and scales; giving it several means it rarely has
/// to. These cover the panel heights in ordinary use and their 2x equivalents.
pub const SIZES: [i32; 5] = [16, 22, 24, 32, 48];

/// Every size, for `Tray::icon_pixmap`.
#[must_use]
pub fn pixmaps(state: State) -> Vec<Icon> {
    SIZES.iter().map(|&size| render(state, size)).collect()
}

/// Draw one icon at one size.
#[must_use]
pub fn render(state: State, size: i32) -> Icon {
    let size = size.clamp(8, 512);
    let n = (size * size) as usize;
    let mut buf = vec![[0.0f32; 4]; n];

    let (stroke, rgb) = match state {
        State::Idle => (0.9, [0x7c, 0xb8, 0xe4]),
        State::Holding => (2.0, [0x1a, 0x7f, 0xc4]),
        State::Snoozed => (0.95, [0x8a, 0x91, 0x99]),
    };

    for rotation in [0.0f32, 60.0, 120.0] {
        for seg in ARM {
            stroke_segment(&mut buf, size, &seg.rotated(rotation), stroke, rgb);
        }
    }

    match state {
        // The hub is what separates "holding" from "idle" at a glance; weight
        // alone was tried against a rendered 22-pixel panel and was not enough.
        State::Holding => fill_disc(&mut buf, size, 8.0, 8.0, 2.6, rgb),
        State::Snoozed => {
            // Bottom right, where a panel overlay badge is conventionally read.
            fill_disc(&mut buf, size, 11.6, 11.6, 4.1, [0x3c, 0x41, 0x47]);
            for x in [10.65f32, 12.55] {
                let bar = Segment { x0: x, y0: 10.3, x1: x, y1: 12.9 };
                stroke_segment(&mut buf, size, &bar, 1.15, [0xe8, 0xeb, 0xee]);
            }
        }
        State::Idle => {}
    }

    Icon { width: size, height: size, data: to_argb(&buf) }
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

impl Segment {
    /// About the centre of the design space.
    fn rotated(self, degrees: f32) -> Self {
        let (s, c) = degrees.to_radians().sin_cos();
        let mid = SPAN / 2.0;
        let turn = |x: f32, y: f32| {
            let (dx, dy) = (x - mid, y - mid);
            (mid + dx * c - dy * s, mid + dx * s + dy * c)
        };
        let (x0, y0) = turn(self.x0, self.y0);
        let (x1, y1) = turn(self.x1, self.y1);
        Self { x0, y0, x1, y1 }
    }

    /// Distance from a point to this segment, both in design units.
    fn distance(&self, px: f32, py: f32) -> f32 {
        let (dx, dy) = (self.x1 - self.x0, self.y1 - self.y0);
        let len2 = dx * dx + dy * dy;
        // A zero-length segment is a point, and dividing by its length is how a
        // rounded cap becomes a NaN that paints the whole icon.
        let t = if len2 <= f32::EPSILON {
            0.0
        } else {
            (((px - self.x0) * dx + (py - self.y0) * dy) / len2).clamp(0.0, 1.0)
        };
        let (cx, cy) = (self.x0 + t * dx, self.y0 + t * dy);
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt()
    }
}

/// Paint a round-capped line.
fn stroke_segment(buf: &mut [[f32; 4]], size: i32, seg: &Segment, width: f32, rgb: [u8; 3]) {
    paint(buf, size, rgb, |x, y| coverage(seg.distance(x, y), width / 2.0, size));
}

fn fill_disc(buf: &mut [[f32; 4]], size: i32, cx: f32, cy: f32, r: f32, rgb: [u8; 3]) {
    paint(buf, size, rgb, |x, y| coverage(((x - cx).powi(2) + (y - cy).powi(2)).sqrt(), r, size));
}

/// How much of a pixel a shape covers, from its distance to the edge.
///
/// One pixel of feathering either side of the boundary, which is what keeps a
/// 16-pixel flake from looking like a stack of stairs. The pixel width in design
/// units is `SPAN / size`, so this sharpens automatically as the panel asks for
/// bigger icons.
fn coverage(distance: f32, radius: f32, size: i32) -> f32 {
    let pixel = SPAN / size as f32;
    (0.5 + (radius - distance) / pixel).clamp(0.0, 1.0)
}

/// Composite one shape over the buffer, source-over, straight alpha.
fn paint(buf: &mut [[f32; 4]], size: i32, rgb: [u8; 3], cov: impl Fn(f32, f32) -> f32) {
    let scale = SPAN / size as f32;
    for py in 0..size {
        for px in 0..size {
            // Pixel centres, so a shape centred on the design space lands
            // symmetrically rather than half a pixel off.
            let x = (px as f32 + 0.5) * scale;
            let y = (py as f32 + 0.5) * scale;
            let a = cov(x, y);
            if a <= 0.0 {
                continue;
            }
            let dst = &mut buf[(py * size + px) as usize];
            let out_a = a + dst[3] * (1.0 - a);
            if out_a <= 0.0 {
                continue;
            }
            for (i, c) in rgb.iter().enumerate() {
                let src = f32::from(*c) / 255.0;
                dst[i] = (src * a + dst[i] * dst[3] * (1.0 - a)) / out_a;
            }
            dst[3] = out_a;
        }
    }
}

/// ARGB32 in network byte order, which is what the StatusNotifierItem spec asks
/// for: alpha first, then red, green, blue.
fn to_argb(buf: &[[f32; 4]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len() * 4);
    for p in buf {
        let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        out.push(byte(p[3]));
        out.push(byte(p[0]));
        out.push(byte(p[1]));
        out.push(byte(p[2]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Alpha of one pixel, by design-space coordinate.
    fn alpha_at(icon: &Icon, x: f32, y: f32) -> u8 {
        let px = ((x / SPAN) * icon.width as f32) as i32;
        let py = ((y / SPAN) * icon.height as f32) as i32;
        let i = ((py * icon.width + px) * 4) as usize;
        icon.data[i]
    }

    #[test]
    fn the_buffer_is_the_size_and_the_format_the_spec_asks_for() {
        for size in SIZES {
            let icon = render(State::Idle, size);
            assert_eq!(icon.width, size);
            assert_eq!(icon.height, size);
            assert_eq!(icon.data.len(), (size * size * 4) as usize, "ARGB32, four per pixel");
        }
    }

    #[test]
    fn the_corners_are_transparent_and_the_arms_are_not() {
        // The failure this catches is the one that looks identical to "no icon
        // at all" in a panel: an all-zero buffer.
        let icon = render(State::Idle, 48);
        assert_eq!(alpha_at(&icon, 0.5, 0.5), 0, "the corner is outside the flake");
        assert!(alpha_at(&icon, 8.0, 3.0) > 128, "the vertical arm is drawn");
        assert!(alpha_at(&icon, 8.0, 13.0) > 128, "and runs the full diameter");
    }

    #[test]
    fn holding_has_a_solid_hub_and_idle_does_not() {
        // Weight alone was not enough to tell the two apart at panel size, so
        // the hub is the load-bearing difference and worth pinning.
        let holding = render(State::Holding, 48);
        let idle = render(State::Idle, 48);
        // A point inside the hub but off every arm and chevron.
        assert!(alpha_at(&holding, 9.6, 8.0) > 200, "the hub is filled");
        assert!(alpha_at(&idle, 9.6, 8.0) < 64, "and idle is open there");
    }

    #[test]
    fn snoozed_carries_a_badge_in_the_corner() {
        let snoozed = render(State::Snoozed, 48);
        assert!(alpha_at(&snoozed, 11.6, 11.6) > 200, "the badge is opaque");
        // And the ordinary states leave that corner to the flake alone, which is
        // mostly empty there.
        assert!(alpha_at(&render(State::Idle, 48), 13.5, 13.5) < 64);
    }

    #[test]
    fn the_flake_is_six_fold_symmetric() {
        // The rotation is the only thing making this a snowflake rather than a
        // cross, and getting the centre of rotation wrong produces something
        // that still looks plausible in a 16-pixel thumbnail.
        let icon = render(State::Idle, 64);
        let centre = SPAN / 2.0;
        for (x, y) in [(8.0f32, 2.5f32), (8.0, 5.0), (6.5, 3.6)] {
            let (dx, dy) = (x - centre, y - centre);
            let (s, c) = 180.0f32.to_radians().sin_cos();
            let opposite = (centre + dx * c - dy * s, centre + dx * s + dy * c);
            let here = alpha_at(&icon, x, y);
            let there = alpha_at(&icon, opposite.0, opposite.1);
            assert!(here.abs_diff(there) < 90, "({x},{y}) is {here} but its mirror is {there}");
        }
    }

    /// Not an assertion: writes the icons out so a human can look at them.
    ///
    /// `DOZED_ICON_OUT=/tmp cargo test -p dozed --bin dozed emit_ -- --ignored`
    ///
    /// The reason this exists rather than a screenshot in the repository: the
    /// only test that mattered for these was "can you tell the three apart at 22
    /// pixels on a dark panel", and the first attempt failed it. Netpbm because
    /// it needs no encoder — `magick a.ppm a-a.pgm -alpha off -compose
    /// CopyOpacity -composite out.png` turns a pair into something viewable.
    #[test]
    #[ignore = "writes images for eyeballing rather than asserting anything"]
    fn emit_the_icons_for_a_human_to_look_at() {
        let dir = std::env::var("DOZED_ICON_OUT").unwrap_or_else(|_| "/tmp".into());
        for (name, state) in
            [("idle", State::Idle), ("holding", State::Holding), ("snoozed", State::Snoozed)]
        {
            for size in [22, 48] {
                let icon = render(state, size);
                let mut rgb = format!("P6\n{size} {size}\n255\n").into_bytes();
                let mut alpha = format!("P5\n{size} {size}\n255\n").into_bytes();
                for px in icon.data.as_chunks::<4>().0 {
                    rgb.extend_from_slice(&[px[1], px[2], px[3]]);
                    alpha.push(px[0]);
                }
                std::fs::write(format!("{dir}/{name}-{size}.ppm"), rgb).unwrap();
                std::fs::write(format!("{dir}/{name}-{size}-a.pgm"), alpha).unwrap();
            }
        }
    }

    #[test]
    fn a_degenerate_size_cannot_produce_a_broken_buffer() {
        // A panel asking for something absurd must not divide by zero into a
        // NaN, which paints every pixel and shows as a solid block.
        for size in [0, 1, -4, 4096] {
            let icon = render(State::Snoozed, size);
            assert_eq!(icon.data.len(), (icon.width * icon.height * 4) as usize);
            assert!(icon.width >= 8 && icon.width <= 512, "clamped to {}", icon.width);
        }
    }
}
