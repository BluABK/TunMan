//! Country flags as actual images.
//!
//! The obvious way to draw a flag is the regional-indicator emoji pair, and on
//! Windows it does not work: the platform's emoji font has no flag glyphs at
//! all, so a flag emoji renders as two letters in boxes. egui's own bundled
//! emoji font is monochrome, which cannot help either. A flag has to be a
//! picture.
//!
//! All 252 of them are compiled into the binary from `assets/flags/` — 74 KiB
//! for the set, at 40 px wide, which is nothing next to depending on a website
//! being reachable. That matters more here than in most apps: this one manages
//! network plumbing, so the times someone is staring at its table are
//! disproportionately the times the network is broken.
//!
//! The images are flat rectangles rather than the waving variety, and come from
//! [flagpedia](https://flagpedia.net), rendered from Wikimedia Commons vectors
//! and in the public domain.

include!(concat!(env!("OUT_DIR"), "/flags_table.rs"));

/// The two-letter code in the form the table is keyed by, or `None` when it is
/// not a country code at all.
///
/// `XX` is the "unknown" the geo probe returns when Cloudflare gives no `loc`.
pub fn normalise(country: &str) -> Option<String> {
    let c = country.trim().to_ascii_lowercase();
    let ok = c.len() == 2 && c.bytes().all(|b| b.is_ascii_lowercase()) && c != "xx";
    ok.then_some(c)
}

/// The bundled PNG for a country, if there is one.
pub fn png(country: &str) -> Option<&'static [u8]> {
    let cc = normalise(country)?;
    let i = FLAGS.binary_search_by(|(code, _)| (*code).cmp(cc.as_str())).ok()?;
    Some(FLAGS[i].1)
}

/// Decode a PNG into something egui can upload.
///
/// Its own function so the failure is contained: a truncated or corrupt cache
/// file returns `None` and the row falls back to letters, rather than the UI
/// panicking on someone's half-written download.
pub fn decode(png: &[u8]) -> Option<egui::ColorImage> {
    // Cursor, not the slice: png 0.18 wants Seek as well as BufRead.
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    // Flags are mostly flat colour, so they are served as PALETTE images at 1-4
    // bits per pixel — the real ones from the CDN are 2-bit indexed. Handling
    // only RGB and RGBA would have meant decoding nothing at all, which is
    // exactly the shape of bug that hides behind "the image just did not show
    // up". This expands palette and low bit depths to plain 8-bit channels, so
    // the arms below are the only ones that can occur.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    let (w, h) = (info.width as usize, info.height as usize);
    let data = &buf[..info.buffer_size()];

    let pixels: Vec<egui::Color32> = match info.color_type {
        png::ColorType::Rgba => data.chunks_exact(4).map(px_rgba).collect(),
        png::ColorType::Rgb => {
            data.chunks_exact(3).map(|p| egui::Color32::from_rgb(p[0], p[1], p[2])).collect()
        }
        png::ColorType::GrayscaleAlpha => data
            .chunks_exact(2)
            .map(|p| egui::Color32::from_rgba_unmultiplied(p[0], p[0], p[0], p[1]))
            .collect(),
        png::ColorType::Grayscale => {
            data.iter().map(|g| egui::Color32::from_rgb(*g, *g, *g)).collect()
        }
        // Normalised away above; unreachable in practice, and a blank rather
        // than a panic if a future png release changes what it emits.
        png::ColorType::Indexed => return None,
    };
    if pixels.len() != w * h {
        return None;
    }
    Some(egui::ColorImage::new([w, h], pixels))
}

fn px_rgba(p: &[u8]) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(p[0], p[1], p[2], p[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_country_code_is_two_letters_and_nothing_else() {
        assert_eq!(normalise("DE").as_deref(), Some("de"));
        assert_eq!(normalise(" no ").as_deref(), Some("no"));
        assert_eq!(normalise("se").as_deref(), Some("se"));
        // The probe's own "unknown", and things that are not codes.
        assert_eq!(normalise("XX"), None);
        assert_eq!(normalise(""), None);
        assert_eq!(normalise("Norway"), None);
        assert_eq!(normalise("d3"), None);
    }

    /// The set is complete enough to be worth bundling, and keyed the way the
    /// lookup expects.
    #[test]
    fn every_bundled_flag_is_a_lowercase_two_letter_code() {
        assert!(FLAGS.len() > 200, "only {} flags bundled", FLAGS.len());
        for (cc, bytes) in FLAGS {
            assert_eq!(cc.len(), 2, "not a country code: {cc}");
            assert!(cc.bytes().all(|b| b.is_ascii_lowercase()), "not lowercase: {cc}");
            assert!(!bytes.is_empty(), "{cc} is empty");
            assert_eq!(&bytes[..4], b"\x89PNG", "{cc} is not a PNG");
        }
    }

    /// The lookup is a binary search, so the table has to be sorted — and a
    /// table that is not would fail by quietly missing flags, not by erroring.
    #[test]
    fn the_table_is_sorted() {
        assert!(FLAGS.windows(2).all(|w| w[0].0 < w[1].0), "the flag table is out of order");
    }

    #[test]
    fn a_country_code_finds_its_flag() {
        for cc in ["se", "NO", " de ", "nl", "us", "jp"] {
            assert!(png(cc).is_some(), "no flag for {cc}");
        }
        for missing in ["XX", "", "Norway", "zz"] {
            assert!(png(missing).is_none(), "unexpected flag for {missing:?}");
        }
    }

    /// Every bundled flag has to survive the decoder, not just the ones that
    /// happen to be looked at. They are palette PNGs at four different bit
    /// depths, and a decoder gap shows up as an image that silently never
    /// appears.
    #[test]
    fn every_bundled_flag_decodes() {
        for (cc, bytes) in FLAGS {
            let img = decode(bytes).unwrap_or_else(|| panic!("{cc} does not decode"));
            assert!(img.size[0] > 0 && img.size[1] > 0, "{cc} decoded to nothing");
            assert_eq!(img.pixels.len(), img.size[0] * img.size[1], "{cc} pixel count");
        }
    }

    /// Round-trip a real PNG, since the whole feature is "this file becomes
    /// those pixels".
    #[test]
    fn a_png_becomes_an_image() {
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        }
        let img = decode(&png_bytes).expect("decodes");
        assert_eq!(img.size, [2, 1]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgba_unmultiplied(255, 0, 0, 255));
        assert_eq!(img.pixels[1], egui::Color32::from_rgba_unmultiplied(0, 0, 255, 128));
    }

    /// What the CDN actually serves: a low-bit-depth PALETTE image, because a
    /// flag is a few flat colours. Decoding only RGB and RGBA meant decoding no
    /// real flag at all — caught by fetching one and looking, which is the only
    /// way this kind of gap ever shows up.
    #[test]
    fn a_palette_png_decodes_too() {
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
            enc.set_color(png::ColorType::Indexed);
            enc.set_depth(png::BitDepth::Two);
            // Sweden's colours, as it happens.
            enc.set_palette(vec![0x00, 0x6a, 0xa7, 0xfe, 0xcc, 0x00]);
            let mut w = enc.write_header().unwrap();
            // Two 2-bit pixels packed into one byte, high bits first: index 0
            // then index 1.
            w.write_image_data(&[0b0001_0000]).unwrap();
        }
        let img = decode(&png_bytes).expect("a palette png decodes");
        assert_eq!(img.size, [2, 1]);
        assert_eq!(img.pixels[0], egui::Color32::from_rgb(0x00, 0x6a, 0xa7));
        assert_eq!(img.pixels[1], egui::Color32::from_rgb(0xfe, 0xcc, 0x00));
    }

    /// A half-written cache file must not take the UI down with it.
    #[test]
    fn a_broken_png_is_just_no_image() {
        assert!(decode(b"").is_none());
        assert!(decode(b"not a png at all").is_none());
        assert!(decode(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).is_none());
    }
}
