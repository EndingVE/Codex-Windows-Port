//! Rasterises the tray icons at runtime.
//!
//! Kept in Rust (not a static PNG) so the tray mark can carry live state: the
//! arc length is the provider's used percentage, the colour follows the shared
//! severity scale, and — with `showPercentInIcon` — the rounded number is drawn
//! into the disc as well. CodexBar's macOS status item is an 18×18 template
//! image; a Windows tray slot is 16 logical px, so we draw at 32 and let the
//! shell resize.
//!
//! Two shapes exist:
//!  * [`gauge_rgba`] — the original ring, used when `showPercentInIcon` is off;
//!  * [`percent_rgba`] — ring **plus** the percentage in a 3×5 bitmap font.
//!
//! The exact RGBA buffers handed to `TrayIcon::set_icon` are what
//! `codexbar-win --dump-icons <dir>` writes to PNG, which is how the evidence
//! for "the icon really carries the number" is produced.

use codexbar_core::{severity_for_used_percent, Severity};

/// RGBA colour for a severity bucket. Matches the CSS custom properties in `ui/styles.css`.
pub const fn severity_rgb(used_percent: f64) -> [u8; 4] {
    match severity_for_used_percent(used_percent) {
        Severity::Healthy => [34, 197, 94, 255],
        Severity::Warning => [245, 158, 11, 255],
        Severity::Critical => [239, 68, 68, 255],
    }
}

/// Track colour drawn behind the arc, the same for every severity.
const TRACK: [u8; 4] = [58, 63, 74, 255];
/// Hub fill so the mark stays legible in a 16 px tray.
const HUB: [u8; 4] = [30, 33, 40, 255];
/// Digit colour.
const INK: [u8; 4] = [245, 247, 250, 255];
/// Neutral accent for "no data" icons.
const UNKNOWN_ACCENT: [u8; 4] = [120, 128, 140, 255];

/// 3×5 bitmap glyphs. Each `u8` is one row, bit 2 = leftmost pixel.
fn glyph(ch: char) -> Option<[u8; 5]> {
    Some(match ch {
        '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
        '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
        '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
        '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
        '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
        '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
        '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
        '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
        '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
        '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
        '-' => [0b000, 0b000, 0b111, 0b000, 0b000],
        '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
        _ => return None,
    })
}

/// Glyph cell is 3 wide + 1 column of spacing.
fn text_width(chars: usize, scale: u32) -> u32 {
    if chars == 0 {
        return 0;
    }
    (chars as u32 * 4 - 1) * scale
}

fn text_height(scale: u32) -> u32 {
    5 * scale
}

/// Largest integer scale that keeps `chars` glyphs inside `avail`×`avail`,
/// capped so a single digit does not turn into a wall of pixels.
fn fit_scale(chars: usize, avail: u32) -> u32 {
    let mut best = 1;
    for scale in 1..=6u32 {
        if text_width(chars, scale) <= avail && text_height(scale) <= avail {
            best = scale;
        }
    }
    best
}

/// Stamp `text` centred in a `size`×`size` RGBA buffer.
fn draw_centered_text(out: &mut [u8], size: u32, text: &str, scale: u32, color: [u8; 4]) {
    let chars: Vec<char> = text.chars().collect();
    let width = text_width(chars.len(), scale);
    let height = text_height(scale);
    if width == 0 || width > size || height > size {
        return;
    }
    let x0 = (size - width) / 2;
    let y0 = (size - height) / 2;

    for (index, ch) in chars.iter().enumerate() {
        let Some(rows) = glyph(*ch) else { continue };
        let cell_x = x0 + index as u32 * 4 * scale;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..3u32 {
                if bits & (1 << (2 - col)) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        let x = cell_x + col * scale + dx;
                        let y = y0 + row as u32 * scale + dy;
                        if x >= size || y >= size {
                            continue;
                        }
                        let idx = ((y * size + x) * 4) as usize;
                        out[idx..idx + 4].copy_from_slice(&color);
                    }
                }
            }
        }
    }
}

/// Draw a donut gauge. `used_percent` is `0..=100` (values outside are clamped).
///
/// Returns tightly packed RGBA bytes, row-major, `size * size * 4` long.
pub fn gauge_rgba(size: u32, used_percent: f64) -> Vec<u8> {
    let size = size.max(8);
    let accent = severity_rgb(used_percent);
    let fraction = (used_percent / 100.0).clamp(0.0, 1.0);

    let center = (size as f32 - 1.0) / 2.0;
    let outer = size as f32 * 0.5 - size as f32 * 0.06;
    let inner = outer - (size as f32 * 0.20 * 0.5).max(1.5);
    let sweep = 360.0 * fraction as f32;
    let hub_r = ((size as f32) * 0.06).max(1.0);

    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();

            let pixel: Option<[u8; 4]> = if dist > outer {
                None
            } else if dist < hub_r {
                Some([255, 255, 255, 255])
            } else if dist < inner {
                Some(HUB)
            } else {
                // atan2 gives -180..180 with 0 = 3 o'clock; shift so 0 = 12 o'clock.
                let angle = (dy.atan2(dx).to_degrees() + 450.0) % 360.0;
                if angle <= sweep {
                    Some(accent)
                } else {
                    Some(TRACK)
                }
            };

            if let Some(px) = pixel {
                let idx = ((y * size + x) * 4) as usize;
                out[idx..idx + 4].copy_from_slice(&px);
            }
        }
    }
    out
}

/// Ring + the used percentage drawn in the middle.
///
/// Geometry is a solid disc (so the digits always have contrast) with a thicker
/// ring around it: the ring is the gauge, the number is the read-out. At
/// `size = 32` that leaves a ~22 px hole, which fits `42` at scale 3, `7` at
/// scale 4 and `100` at scale 2.
pub fn percent_rgba(size: u32, used_percent: f64) -> Vec<u8> {
    let size = size.max(16);
    let accent = severity_rgb(used_percent);
    let fraction = (used_percent / 100.0).clamp(0.0, 1.0);

    let center = (size as f32 - 1.0) / 2.0;
    let outer = size as f32 * 0.5 - size as f32 * 0.045;
    let ring = (size as f32 * 0.11).max(2.0);
    let inner = (outer - ring).max(2.0);
    let sweep = 360.0 * fraction as f32;

    let mut out = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > outer {
                continue; // stays transparent
            }
            let pixel = if dist >= inner {
                let angle = (dy.atan2(dx).to_degrees() + 450.0) % 360.0;
                if angle <= sweep {
                    accent
                } else {
                    TRACK
                }
            } else {
                HUB
            };
            let idx = ((y * size + x) * 4) as usize;
            out[idx..idx + 4].copy_from_slice(&pixel);
        }
    }

    let text = format!("{}", used_percent.clamp(0.0, 100.0).round() as i64);
    let scale = fit_scale(text.chars().count(), (inner * 2.0).ceil() as u32);
    draw_centered_text(&mut out, size, &text, scale, INK);
    out
}

/// Icon for a provider with no usable number: neutral ring with a dash.
pub fn unknown_rgba(size: u32) -> Vec<u8> {
    let size = size.max(16);
    let mut out = percent_rgba(size, 0.0);
    // Replace the "0" with a dash: repaint the hole, then stamp the glyph.
    let center = (size as f32 - 1.0) / 2.0;
    let outer = size as f32 * 0.5 - size as f32 * 0.045;
    let inner = (outer - (size as f32 * 0.11).max(2.0)).max(2.0);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist >= inner {
                continue;
            }
            let idx = ((y * size + x) * 4) as usize;
            out[idx..idx + 4].copy_from_slice(&HUB);
        }
    }
    // Grey, short sweep: "we have no idea".
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist < inner || dist > outer {
                continue;
            }
            let angle = (dy.atan2(dx).to_degrees() + 450.0) % 360.0;
            if angle > 60.0 {
                let idx = ((y * size + x) * 4) as usize;
                out[idx..idx + 4].copy_from_slice(&UNKNOWN_ACCENT);
            }
        }
    }
    let scale = fit_scale(1, (inner * 2.0).ceil() as u32);
    draw_centered_text(&mut out, size, "-", scale, INK);
    out
}

/// The single entry point the tray uses: pick the shape from settings + state.
///
/// `percent = None` means the provider has no usable number (not configured,
/// error, or only synthetic placeholder lanes).
pub fn icon_rgba(size: u32, percent: Option<f64>, show_percent: bool) -> Vec<u8> {
    match (percent, show_percent) {
        (None, _) => unknown_rgba(size),
        (Some(pct), true) => percent_rgba(size, pct),
        (Some(pct), false) => gauge_rgba(size, pct),
    }
}

/// See [`icon_rgba`].
pub fn icon_image(
    size: u32,
    percent: Option<f64>,
    show_percent: bool,
) -> tauri::image::Image<'static> {
    let rgba = icon_rgba(size, percent, show_percent);
    tauri::image::Image::new_owned(rgba, size, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_fully_transparent_corners() {
        let rgba = gauge_rgba(32, 50.0);
        assert_eq!(rgba.len(), 32 * 32 * 4);
        assert_eq!(
            &rgba[0..4],
            &[0, 0, 0, 0],
            "top-left corner must stay transparent"
        );
    }

    #[test]
    fn colour_follows_severity_scale() {
        assert_eq!(severity_rgb(10.0), [34, 197, 94, 255]);
        assert_eq!(severity_rgb(75.0), [245, 158, 11, 255]);
        assert_eq!(severity_rgb(95.0), [239, 68, 68, 255]);
    }

    #[test]
    fn clamped_for_out_of_range_input() {
        let a = gauge_rgba(16, -5.0);
        let b = gauge_rgba(16, 0.0);
        assert_eq!(a, b);
    }

    #[test]
    fn percent_icon_differs_from_gauge_and_from_other_values() {
        let gauge = gauge_rgba(32, 42.0);
        let pct = percent_rgba(32, 42.0);
        assert_eq!(pct.len(), 32 * 32 * 4);
        assert_ne!(gauge, pct, "the number must change the pixels");
        assert_ne!(
            percent_rgba(32, 42.0),
            percent_rgba(32, 91.0),
            "different percentages must render differently"
        );
    }

    #[test]
    fn percent_icon_paints_ink_pixels_near_the_centre() {
        let rgba = percent_rgba(32, 42.0);
        let centre = 16usize;
        let mut ink = 0;
        for y in centre - 8..centre + 8 {
            for x in centre - 10..centre + 10 {
                let idx = (y * 32 + x) * 4;
                if rgba[idx..idx + 4] == INK {
                    ink += 1;
                }
            }
        }
        assert!(ink > 20, "expected glyph pixels in the hub, found {ink}");
    }

    #[test]
    fn three_digits_still_fit_inside_the_hole() {
        let hole = {
            let size = 32u32;
            let outer = size as f32 * 0.5 - size as f32 * 0.045;
            let inner = outer - (size as f32 * 0.11).max(2.0);
            (inner * 2.0).ceil() as u32
        };
        let scale = fit_scale(3, hole);
        assert!(text_width(3, scale) <= hole, "100 must fit");
        assert!(text_height(scale) <= hole);
    }

    #[test]
    fn unknown_icon_is_not_the_zero_icon() {
        assert_ne!(unknown_rgba(32), percent_rgba(32, 0.0));
    }
}
