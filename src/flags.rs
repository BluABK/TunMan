//! Country flags as actual images.
//!
//! The obvious way to draw a flag is the regional-indicator emoji pair, and on
//! Windows it does not work: the platform's emoji font has no flag glyphs at
//! all, so 🇩🇪 renders as the letters D and E in boxes. egui's own bundled emoji
//! font is monochrome, which cannot help either. A flag has to be a picture.
//!
//! Shipping ~250 of them is a lot of bytes for a table that will ever show two
//! or three, so they are fetched on demand from [flagcdn.com] — one small PNG
//! per country, the first time that country is seen — and kept in
//! `%APPDATA%\TunMan\flags`. After that it is a local file read, offline
//! included. Nothing else about the app depends on this: a fetch that fails
//! leaves the row showing the two-letter code, which is what it showed before.
//!
//! What leaves the machine is one request naming a country code, to a CDN, once
//! per country ever seen. It does not say which tunnel, and it does not repeat.
//!
//! [flagcdn.com]: https://flagcdn.com

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::LazyLock;

use parking_lot::Mutex;

/// Width to fetch. Twice the ~20px the table draws, so the image still looks
/// like a flag rather than a smear on a high-DPI display.
const FETCH_WIDTH: u32 = 40;

/// Codes already being fetched, so a repainting UI asks once rather than sixty
/// times a second.
static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Where the fetched PNGs live.
pub fn cache_dir() -> PathBuf {
    crate::app_paths::data_dir().join("flags")
}

/// The two-letter code in the form used for lookups, or `None` when it is not
/// a country code at all.
///
/// Everything else here is built from this, and it is why the code can be
/// pasted into a URL and a file path without further thought: two ASCII
/// letters, lowercased. `XX` is the "unknown" the geo probe returns.
pub fn normalise(country: &str) -> Option<String> {
    let c = country.trim().to_ascii_lowercase();
    let ok = c.len() == 2 && c.bytes().all(|b| b.is_ascii_lowercase()) && c != "xx";
    ok.then_some(c)
}

/// Where a country's flag is cached.
pub fn cache_path(cc: &str) -> Option<PathBuf> {
    Some(cache_dir().join(format!("{}.png", normalise(cc)?)))
}

/// The CDN URL for a country's flag.
pub fn url(cc: &str) -> Option<String> {
    Some(format!("https://flagcdn.com/w{FETCH_WIDTH}/{}.png", normalise(cc)?))
}

/// The cached PNG for a country, if it has been fetched.
pub fn cached(cc: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_path(cc)?).ok()
}

/// Fetch a flag if it is not cached yet. Returns immediately; the image appears
/// on a later frame.
///
/// Silent on failure by design. A missing flag is a cosmetic gap next to a row
/// that still says which country it is, and an app that made noise about the
/// flag CDN being unreachable would be reporting on itself rather than on the
/// tunnels.
pub fn ensure(cc: &str, rt: &tokio::runtime::Handle) {
    let Some(cc) = normalise(cc) else { return };
    let Some(path) = cache_path(&cc) else { return };
    if path.exists() || !IN_FLIGHT.lock().insert(cc.clone()) {
        return;
    }
    let Some(url) = url(&cc) else { return };
    rt.spawn(async move {
        let fetched = fetch(&url).await;
        match fetched {
            Ok(bytes) if !bytes.is_empty() => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Err(e) = std::fs::write(&path, &bytes) {
                    tracing::debug!("could not cache the {cc} flag: {e}");
                }
            }
            Ok(_) => tracing::debug!("the {cc} flag came back empty"),
            Err(e) => tracing::debug!("could not fetch the {cc} flag: {e}"),
        }
        IN_FLIGHT.lock().remove(&cc);
    });
}

async fn fetch(url: &str) -> anyhow::Result<Vec<u8>> {
    // Direct, not through a tunnel: this is a picture of a flag from a CDN, and
    // routing it through someone's VPS would spend their bandwidth to hide
    // nothing. Short timeout — it is decoration.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("TunMan/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.bytes().await?.to_vec())
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

    /// The code goes into a URL and a file path, so anything that is not two
    /// letters has to be rejected before it gets there — not sanitised after.
    #[test]
    fn nothing_but_a_country_code_reaches_a_url_or_a_path() {
        for bad in ["../etc", "a/b", "..", "%2e%2e", "de/../../x", "\\\\server\\share"] {
            assert_eq!(url(bad), None, "url for {bad:?}");
            assert_eq!(cache_path(bad), None, "path for {bad:?}");
        }
        assert_eq!(url("DE").as_deref(), Some("https://flagcdn.com/w40/de.png"));
        assert!(cache_path("DE").unwrap().ends_with("de.png"));
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
