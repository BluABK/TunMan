// The app icon as pixels, drawn from a description rather than shipped as a
// file — and with **no dependencies**, because `build.rs` includes this same
// source.
//
// That sharing is the point. Windows takes the icon for a shortcut, an
// Explorer listing, and the taskbar button of a program launched from a
// shortcut out of the executable's own resources; the icon a running app
// hands to its window is a separate thing that Explorer never sees. So there
// are two consumers, and if they were drawn twice they would drift. Here the
// build script renders this drawing into the `.ico` it embeds, and the app
// renders the same drawing for its window and tray.
//
// The design: a dark tunnel portal on cyan, deliberately unlike
// StreamArchiver's purple tile with a red record dot. The two sit in the same
// tray, and telling them apart at 16 px matters more than detail does.

/// Sizes embedded in the `.ico`. Windows picks per context — 16 for menus and
/// the title bar, 32 for the taskbar, 48 for Explorer's medium icons, 256 for
/// its extra-large ones. A size that is not present is scaled from one that
/// is, which at small sizes looks like a smear.
///
/// Read by `build.rs`, not by the app.
#[allow(dead_code)]
pub const SIZES: &[u32] = &[16, 20, 24, 32, 48, 64, 128, 256];

/// The cyan field.
const CYAN: [u8; 3] = [0x22, 0xd3, 0xee];
/// The portal's mouth.
const DARK: [u8; 3] = [0x0b, 0x16, 0x20];

/// The whole design, in a 32×32 coordinate space. `None` is transparent.
///
/// One function, called at whatever density the caller wants, is what lets the
/// same shape come out right at 16 px and at 256.
fn sample(fx: f32, fy: f32) -> Option<[u8; 3]> {
    const N: f32 = 32.0;
    const CORNER: f32 = 5.0;
    // Portal: a semicircle sitting on a rectangle, open at the bottom edge, so
    // it still reads as an arch when scaled down.
    const AX: f32 = 16.0;
    const AY: f32 = 15.0;
    const AR: f32 = 7.0;
    const FLOOR: f32 = 27.0;

    // Rounded-corner mask: clamp the point into the inner rect and measure how
    // far outside it fell.
    let cx = fx.clamp(CORNER, N - CORNER);
    let cy = fy.clamp(CORNER, N - CORNER);
    if ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() > CORNER {
        return None;
    }

    let in_arch = if fy < AY {
        ((fx - AX).powi(2) + (fy - AY).powi(2)).sqrt() <= AR
    } else {
        (fx - AX).abs() <= AR && fy <= FLOOR
    };
    Some(if in_arch { DARK } else { CYAN })
}

/// The icon at `size`, as RGBA8.
///
/// Supersampled: at 16 px the arch is four pixels wide and hard edges turn it
/// into a staircase, which is exactly the size where the icon has to be
/// recognised at a glance.
pub fn rgba(size: u32) -> Vec<u8> {
    /// Samples per axis, per pixel.
    const SS: u32 = 4;

    let mut out = vec![0u8; (size * size * 4) as usize];
    let scale = size as f32 / 32.0;
    let n = (SS * SS) as f32;
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut g, mut b, mut covered) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let fx = (x as f32 + (sx as f32 + 0.5) / SS as f32) / scale;
                    let fy = (y as f32 + (sy as f32 + 0.5) / SS as f32) / scale;
                    if let Some(c) = sample(fx, fy) {
                        r += c[0] as f32;
                        g += c[1] as f32;
                        b += c[2] as f32;
                        covered += 1.0;
                    }
                }
            }
            if covered == 0.0 {
                continue; // fully transparent, and the buffer is already zero
            }
            let i = ((y * size + x) * 4) as usize;
            // Colour is the average of the samples that hit something; alpha is
            // how many of them did. Averaging colour over the misses instead
            // would darken every edge towards black.
            out[i] = (r / covered).round() as u8;
            out[i + 1] = (g / covered).round() as u8;
            out[i + 2] = (b / covered).round() as u8;
            out[i + 3] = (255.0 * covered / n).round() as u8;
        }
    }
    out
}

/// The icon as a Windows `.ico` holding every size in `sizes`.
///
/// Used by `build.rs`, not by the app itself.
#[allow(dead_code)]
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|s| dib(*s)).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    // Directory entries come first, so every image offset is past all of them.
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, data) in sizes.iter().zip(&images) {
        // 256 is written as 0: the field is one byte, and 256 does not fit.
        let dim = if *s >= 256 { 0u8 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for data in images {
        out.extend_from_slice(&data);
    }
    out
}

/// One icon image as a 32-bit DIB: header, then bottom-up BGRA, then the AND
/// mask that predates alpha and is still structurally required.
#[allow(dead_code)]
fn dib(size: u32) -> Vec<u8> {
    let px = rgba(size);
    // The AND mask is 1 bit per pixel with rows padded to 4 bytes. Left at
    // zero — "opaque everywhere" — because the alpha channel is what actually
    // decides transparency on every Windows that can run this.
    let and_len = size.div_ceil(32) * 4 * size;
    let xor_len = size * size * 4;

    let mut v = Vec::with_capacity(40 + (xor_len + and_len) as usize);
    v.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
    v.extend_from_slice(&(size as i32).to_le_bytes());
    // Height counts the XOR and AND masks together, which is why it is doubled.
    v.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // planes
    v.extend_from_slice(&32u16.to_le_bytes()); // bpp
    v.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    v.extend_from_slice(&(xor_len + and_len).to_le_bytes());
    v.extend_from_slice(&0i32.to_le_bytes()); // pixels/metre x
    v.extend_from_slice(&0i32.to_le_bytes()); // pixels/metre y
    v.extend_from_slice(&0u32.to_le_bytes()); // palette entries used
    v.extend_from_slice(&0u32.to_le_bytes()); // palette entries important

    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            v.extend_from_slice(&[px[i + 2], px[i + 1], px[i], px[i + 3]]);
        }
    }
    v.resize(v.len() + and_len as usize, 0);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_the_size_that_was_asked_for() {
        for s in [16u32, 32, 256] {
            assert_eq!(rgba(s).len(), (s * s * 4) as usize, "at {s}px");
        }
    }

    /// The shape has to survive being drawn small — this is the size the tray
    /// and the title bar use, and a tunnel that reads as a blank tile there is
    /// indistinguishable from every other app.
    #[test]
    fn the_portal_is_still_visible_at_16px() {
        let px = rgba(16);
        let at = |x: u32, y: u32| {
            let i = ((y * 16 + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        // Centre is inside the arch, the upper-left area is the cyan field.
        assert!(at(8, 10)[0] < 0x60, "centre should be dark: {:?}", at(8, 10));
        assert!(at(3, 8)[1] > 0x90, "left edge should be cyan: {:?}", at(3, 8));
        // Corners are rounded away. Not fully transparent at 16px — the corner
        // radius lands mid-pixel there, and the smoothing that makes the shape
        // readable is exactly what leaves a trace of alpha behind.
        assert!(at(0, 0)[3] < 0x60, "corner should be mostly transparent: {:?}", at(0, 0));
    }

    /// Every pixel is either transparent or fully opaque in the middle of a
    /// field; only edges may be partial. A drawing that came out uniformly
    /// semi-transparent would look washed out everywhere.
    #[test]
    fn the_fields_are_opaque() {
        let px = rgba(32);
        let alpha = |x: u32, y: u32| px[(((y * 32 + x) * 4) + 3) as usize];
        assert_eq!(alpha(16, 16), 255);
        assert_eq!(alpha(4, 16), 255);
    }

    /// A malformed `.ico` is not rejected by Windows — it is silently ignored,
    /// and the exe falls back to the generic icon, which is the bug this file
    /// exists to fix. So check the parts a reader would check by hand.
    #[test]
    fn the_ico_container_is_well_formed() {
        let sizes = [16u32, 32, 256];
        let v = ico(&sizes);

        assert_eq!(&v[0..2], &[0, 0], "reserved");
        assert_eq!(&v[2..4], &[1, 0], "type 1 = icon");
        assert_eq!(&v[4..6], &[3, 0], "three images");

        let mut expected_offset = 6 + 16 * sizes.len() as u32;
        for (i, s) in sizes.iter().enumerate() {
            let e = 6 + 16 * i;
            let dim = if *s >= 256 { 0 } else { *s as u8 };
            assert_eq!(v[e], dim, "width byte for {s}");
            assert_eq!(v[e + 1], dim, "height byte for {s}");
            assert_eq!(u16::from_le_bytes([v[e + 6], v[e + 7]]), 32, "bpp for {s}");

            let len = u32::from_le_bytes([v[e + 8], v[e + 9], v[e + 10], v[e + 11]]);
            let off = u32::from_le_bytes([v[e + 12], v[e + 13], v[e + 14], v[e + 15]]);
            assert_eq!(off, expected_offset, "offset for {s}");
            assert_eq!(len, 40 + s * s * 4 + s.div_ceil(32) * 4 * s, "byte length for {s}");
            // The header of each image must say what the directory says.
            let h = off as usize;
            assert_eq!(u32::from_le_bytes(v[h..h + 4].try_into().unwrap()), 40);
            assert_eq!(i32::from_le_bytes(v[h + 4..h + 8].try_into().unwrap()), *s as i32);
            assert_eq!(i32::from_le_bytes(v[h + 8..h + 12].try_into().unwrap()), (s * 2) as i32);
            expected_offset += len;
        }
        assert_eq!(v.len() as u32, expected_offset, "trailing or missing bytes");
    }
}
