//! Making sure the symbols the UI draws actually have glyphs.
//!
//! egui's proportional family is Ubuntu-Light with two emoji fonts behind it,
//! and between them they are missing four characters this app leans on:
//!
//! | char | where |
//! |------|-------|
//! | `↓` U+2193 | throughput columns and the totals strip |
//! | `↑` U+2191 | the same |
//! | `→` U+2192 | sync source → destination, SOCKS destinations in the log |
//! | `▲` U+25B2 | the status dot of a FAILED tunnel or mount |
//!
//! A character with no glyph renders as an empty box, so the rate columns read
//! `□ —` and a failed tunnel looks like an unrecognised shape rather than an
//! alarm. All four are in Hack, which egui already loads for the monospace
//! family — so the fix is to let the proportional family fall back to it. No
//! extra font is loaded and nothing else changes: a fallback is only consulted
//! for characters the fonts ahead of it do not have.

use egui::{FontDefinitions, FontFamily};

/// Name epaint registers the bundled monospace font under.
const MONO: &str = "Hack";

/// Give the proportional family the monospace font as a last resort.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(with_symbol_fallback(FontDefinitions::default()));
}

fn with_symbol_fallback(mut defs: FontDefinitions) -> FontDefinitions {
    if !defs.font_data.contains_key(MONO) {
        // A future egui could rename or drop it. Losing the fallback is a
        // cosmetic problem; panicking over it is not worth it.
        return defs;
    }
    if let Some(family) = defs.families.get_mut(&FontFamily::Proportional)
        && !family.iter().any(|f| f == MONO)
    {
        family.push(MONO.to_owned());
    }
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The characters that have no glyph without the fallback. If egui ever
    /// ships a proportional font that covers them this test still passes — it
    /// asserts the fallback is wired up, not that it is needed.
    const NEEDS_FALLBACK: &[char] = &['↓', '↑', '→', '▲'];

    #[test]
    fn the_proportional_family_falls_back_to_the_mono_font() {
        let defs = with_symbol_fallback(FontDefinitions::default());
        let family = &defs.families[&FontFamily::Proportional];
        assert!(family.contains(&MONO.to_owned()), "got {family:?}");
        assert_eq!(family.last().unwrap(), MONO, "a fallback belongs at the end");
        assert!(!NEEDS_FALLBACK.is_empty());
    }

    #[test]
    fn installing_twice_does_not_stack_the_fallback() {
        let once = with_symbol_fallback(FontDefinitions::default());
        let twice = with_symbol_fallback(once.clone());
        assert_eq!(
            twice.families[&FontFamily::Proportional],
            once.families[&FontFamily::Proportional]
        );
    }

    #[test]
    fn a_font_set_without_the_mono_font_is_left_alone() {
        let mut defs = FontDefinitions::default();
        defs.font_data.remove(MONO);
        let family = defs.families[&FontFamily::Proportional].clone();
        assert_eq!(with_symbol_fallback(defs).families[&FontFamily::Proportional], family);
    }
}
