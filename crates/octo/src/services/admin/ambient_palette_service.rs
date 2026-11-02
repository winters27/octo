//! `AmbientPaletteService`: the palettes of the covers in the fetched-songs log, for the
//! dashboard's light. A picture is read once and its palette kept for as long as Octo runs, so
//! the page asks as often as it likes.
//!
//! The pictures come from `FetchedCovers` (the song's own file, else the cover address saved
//! with it, fetched by Octo), so a song the log saved no address for still lights the page.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};

use octo_media::cover::CoverArtService;
use parking_lot::Mutex;

use super::cover_palette::{self, Palette};

pub struct AmbientPaletteService {
    // By the picture's own bytes: a cover that changes is read again. None is remembered too: a
    // picture with no color is not read again.
    palettes: Mutex<HashMap<u64, Option<Palette>>>,
}

impl AmbientPaletteService {
    const MAX_REMEMBERED: usize = 512;

    pub fn new() -> Self {
        AmbientPaletteService {
            palettes: Mutex::new(HashMap::new()),
        }
    }

    /// The palette of a picture, None for one that cannot be read or has no color.
    pub fn palette_of(&self, picture: &[u8]) -> Option<Palette> {
        if picture.is_empty() {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        picture.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(known) = self.palettes.lock().get(&key) {
            return known.clone();
        }
        // The station covers' reading of a picture: its main colors from a small copy.
        let swatches = CoverArtService::swatches_of(picture);
        let palette = cover_palette::from_swatches(swatches.as_deref());

        let mut palettes = self.palettes.lock();
        if palettes.len() >= Self::MAX_REMEMBERED {
            palettes.clear();
        }
        palettes.insert(key, palette.clone());
        palette
    }
}

impl Default for AmbientPaletteService {
    fn default() -> Self {
        Self::new()
    }
}
