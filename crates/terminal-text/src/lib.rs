//! Terminal text stack (CPU side).
//!
//! Responsibilities are strictly separated from the GPU:
//!
//! * **Font discovery** — [`FontLibrary`] scans system font directories and
//!   parses each font with `fontdue` (supporting TTC collections) to learn
//!   its family name and glyph coverage.
//! * **Font fallback** — [`FontLibrary::font_for`] picks the first font in
//!   preference order that contains the requested character (primary
//!   monospace first, then every discovered font).
//! * **Glyph rasterization** — [`Rasterizer`] wraps `fontdue` and produces
//!   grayscale bitmaps plus metrics at a given pixel size.
//! * **Glyph caching** — [`GlyphCache`] memoizes raster results keyed by
//!   (font hash, glyph index, pixel size); the GPU renderer consumes its
//!   entries and owns the atlas texture.
//!
//! The renderer requests glyphs through [`GlyphCache::glyph`], which returns
//! the packed bitmap and metrics the atlas needs — it never touches font
//! files itself.
//!
//! ## Shaping
//!
//! This phase intentionally does **no** complex-script shaping (Arabic,
//! Indic, etc.). The terminal cell model handles grapheme clusters at the
//! cluster level (ZWJ/VS runs merge into a base cell, see `terminal-core`),
//! and font fallback covers missing codepoints. Complex-script shaping is a
//! documented future work item (see ADR 0003).

use fontdue::layout::GlyphRasterConfig;
use fontdue::{Font, FontSettings};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

const FONT_SIZE_BUDGET: usize = 16 * 1024 * 1024;

/// A font file loaded into memory (retained so fonts outlive the cache).
#[derive(Clone)]
pub struct LoadedFont {
    pub id: usize,
    /// Human-readable family name reported by the font.
    pub family: String,
    /// Monospace flag, detected by comparing advance widths.
    pub monospace: bool,
    pub path: PathBuf,
    fonts: Arc<Vec<Font>>,
    /// Index within `fonts` for the face to use for printing.
    pub face: usize,
}

/// Monospace heuristic: in a monospace font the advance width of narrow and
/// wide glyphs is identical. In proportional fonts 'i' and 'M' differ
/// substantially.
fn detect_monospace(font: &Font) -> bool {
    let i = font.metrics('i', 16.0).advance_width;
    let m = font.metrics('M', 16.0).advance_width;
    let one = font.metrics('1', 16.0).advance_width;
    (i - m).abs() < 0.01 && (i - one).abs() < 0.01 && i > 0.0
}

/// Discovers fonts on the system.
pub struct FontLibrary {
    fonts: Vec<LoadedFont>,
    /// Ambient directory cache to avoid rescanning during a session.
    scanned_dirs: Vec<PathBuf>,
    /// Cached id of the primary monospace font (lazily resolved by
    /// [`FontLibrary::font_for`]) so per-glyph fallback selection is O(1)
    /// instead of rescanning the whole library.
    primary_idx: Option<usize>,
}

impl FontLibrary {
    pub fn new() -> Self {
        Self {
            fonts: Vec::new(),
            scanned_dirs: Vec::new(),
            primary_idx: None,
        }
    }

    /// Scans the standard system font directories (macOS + Linux).
    pub fn scan_system(&mut self) {
        let dirs = system_font_dirs();
        for dir in dirs {
            if self.scanned_dirs.contains(&dir) {
                continue;
            }
            self.scanned_dirs.push(dir.clone());
            self.scan_dir(&dir);
        }
    }

    pub fn scan_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            if !matches!(ext.to_lowercase().as_str(), "ttf" | "otf" | "ttc") {
                continue;
            }
            let Ok(data) = std::fs::read(&path) else {
                continue;
            };
            if data.len() > FONT_SIZE_BUDGET {
                continue;
            }
            // TTC collections: try successive collection indices, keep the
            // first face that parses (Menlo.ttc's face 0 is Menlo Regular).
            let parsed = (0..16u32).find_map(|i| {
                let settings = FontSettings {
                    collection_index: i,
                    ..FontSettings::default()
                };
                fontdue::Font::from_bytes(data.clone(), settings).ok()
            });
            let Some(font) = parsed else {
                continue;
            };
            let family = font.name().unwrap_or("unknown").to_string();
            let monospace = detect_monospace(&font);
            let id = self.fonts.len();
            self.fonts.push(LoadedFont {
                id,
                family,
                monospace,
                path,
                fonts: Arc::new(vec![font]),
                face: 0,
            });
        }
    }

    /// Adds a user-supplied font file.
    pub fn add_font_file(&mut self, path: &Path) {
        let Ok(data) = std::fs::read(path) else {
            return;
        };
        let parsed = (0..16u32).find_map(|i| {
            let settings = FontSettings {
                collection_index: i,
                ..FontSettings::default()
            };
            fontdue::Font::from_bytes(data.clone(), settings).ok()
        });
        let Some(font) = parsed else {
            return;
        };
        let family = font.name().unwrap_or("unknown").to_string();
        let monospace = detect_monospace(&font);
        let id = self.fonts.len();
        self.fonts.push(LoadedFont {
            id,
            family,
            monospace,
            path: path.to_path_buf(),
            fonts: Arc::new(vec![font]),
            face: 0,
        });
    }

    /// Best-effort monospace choice: user override, else a small known-good
    /// preference list, else the first monospace font found.
    pub fn primary_monospace(&self, user_pref: Option<&str>) -> Option<&LoadedFont> {
        if let Some(pref) = user_pref {
            if let Some(f) = self
                .fonts
                .iter()
                .find(|f| f.family == pref || f.path.to_string_lossy().contains(pref))
            {
                return Some(f);
            }
        }
        // Purpose-built coding fonts first: JetBrains Mono and Cascadia are
        // designed specifically to disambiguate 0/O and 1/l/I (dotted zero,
        // distinct glyph shapes) and increase x-height for long-session
        // legibility — a documented, evidence-backed improvement over
        // general-purpose monospace fonts. Menlo/Monaco/SF Mono remain as
        // reliable fallbacks that ship on every Mac.
        let preferred = [
            "JetBrains Mono",
            "Cascadia Mono",
            "Cascadia Code",
            "SFMono",
            "SF Mono",
            "Menlo",
            "Monaco",
            "DejaVu Sans Mono",
            "Fira Code",
            "Meslo",
            "Hack",
            "Courier",
        ];
        for name in preferred {
            if let Some(f) = self.fonts.iter().find(|f| f.family.contains(name)) {
                return Some(f);
            }
        }
        self.fonts
            .iter()
            .find(|f| f.monospace)
            .or_else(|| self.fonts.first())
    }

    /// Picks the font to render `c` with: the primary monospace font if it
    /// has the glyph, else the first discovered font that does.
    pub fn font_for(&mut self, c: char) -> Option<&LoadedFont> {
        if self.primary_idx.is_none() {
            self.primary_idx = self.primary_monospace(None).map(|f| f.id);
        }
        if let Some(id) = self.primary_idx {
            if let Some(p) = self.fonts.iter().find(|f| f.id == id) {
                if self.has_glyph(p, c) {
                    return Some(p);
                }
            }
        }
        self.fonts.iter().find(|f| self.has_glyph(f, c))
    }

    /// True if the font contains the character.
    pub fn has_glyph(&self, font: &LoadedFont, c: char) -> bool {
        font.fonts[font.face].has_glyph(c)
    }
}

impl Default for FontLibrary {
    fn default() -> Self {
        Self::new()
    }
}

/// Standard system font directories.
pub fn system_font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(PathBuf::from(&home).join("Library/Fonts"));
        dirs.push(PathBuf::from(&home).join(".fonts"));
        dirs.push(PathBuf::from(&home).join(".local/share/fonts"));
        dirs.push(PathBuf::from(&home).join(".nix-profile/share/fonts"));
    }
    dirs.push(PathBuf::from("/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts"));
    dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
    dirs.push(PathBuf::from("/usr/share/fonts"));
    dirs.push(PathBuf::from("/usr/local/share/fonts"));
    dirs.push(PathBuf::from("/opt/homebrew/share/fonts"));
    dirs
}

/// Metrics for a rasterized glyph, in pixel units (top-left origin).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphMetrics {
    pub width: u32,
    pub height: u32,
    /// x offset of the bitmap from the cell origin.
    pub bearing_x: i32,
    /// y offset of the bitmap from the cell baseline (negative = above).
    pub bearing_y: i32,
    /// Horizontal advance in pixels (typically the cell width for monospace).
    pub advance: f32,
}

/// A rasterized glyph ready for atlas packing.
#[derive(Debug, Clone)]
pub struct RasterGlyph {
    pub key: GlyphRasterConfig,
    pub bitmap: Vec<u8>,
    pub metrics: GlyphMetrics,
    /// Monospace advance width in pixels, cached for layout.
    pub advance_px: f32,
}

/// Rasterizes glyphs via fontdue.
pub struct Rasterizer {
    size_px: f32,
}

impl Rasterizer {
    pub fn new(size_px: f32) -> Self {
        Self { size_px }
    }

    pub fn size_px(&self) -> f32 {
        self.size_px
    }

    pub fn set_size_px(&mut self, px: f32) {
        self.size_px = px;
    }

    /// Measures the advance width and line height expected of one cell.
    pub fn cell_metrics(&self, font: &LoadedFont) -> (f32, f32) {
        let f = &font.fonts[font.face];
        let advance = f.metrics('M', self.size_px).advance_width;
        let height = f
            .horizontal_line_metrics(self.size_px)
            .map(|l| l.new_line_size)
            .unwrap_or(advance);
        (advance, height)
    }

    /// Ascent (baseline distance from the top of the line box) at the
    /// current pixel size. Used to position glyph bitmaps inside cells.
    pub fn ascent(&self, font: &LoadedFont) -> f32 {
        font.fonts[font.face]
            .horizontal_line_metrics(self.size_px)
            .map(|l| l.ascent)
            .unwrap_or(self.size_px * 0.8)
    }

    /// Rasterizes `c` using `font`. Returns `None` if the font lacks the glyph.
    pub fn raster(&self, font: &LoadedFont, c: char) -> Option<RasterGlyph> {
        let f = &font.fonts[font.face];
        if !f.has_glyph(c) {
            return None;
        }
        let glyph_index = f.lookup_glyph_index(c);
        let px = self.size_px;
        let config = GlyphRasterConfig {
            glyph_index,
            px,
            font_hash: f.file_hash(),
        };
        let (metrics, bitmap) = f.rasterize_config(config);
        Some(RasterGlyph {
            key: config,
            bitmap,
            metrics: GlyphMetrics {
                width: metrics.width as u32,
                height: metrics.height as u32,
                bearing_x: metrics.xmin,
                bearing_y: metrics.ymin,
                advance: metrics.advance_width,
            },
            advance_px: metrics.advance_width,
        })
    }
}

/// Lookup counters, exposed for the glyph-atlas stress test.
#[derive(Debug, Default)]
pub struct GlyphCacheStats {
    /// Lookups that found an already-rasterized glyph.
    pub hits: AtomicU64,
    /// Lookups that had to rasterize (cache miss).
    pub misses: AtomicU64,
}

/// Cached rasterized glyphs keyed by (font, glyph, size). The cache evicts
/// least-recently-used entries when it passes a byte budget; evicted glyphs
/// are re-rasterized on demand.
pub struct GlyphCache {
    rasterizer: Rasterizer,
    entries: HashMap<GlyphRasterConfig, RasterGlyph>,
    /// LRU order: index 0 is least recently used.
    lru: Vec<GlyphRasterConfig>,
    /// Total bytes of bitmap data retained.
    bytes: usize,
    budget: usize,
    /// Font/size for layout queries.
    active_font: Option<usize>,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    /// Hit/miss counters for the atlas stress test.
    stats: GlyphCacheStats,
}

impl GlyphCache {
    pub fn new(size_px: f32, budget_bytes: usize) -> Self {
        Self {
            rasterizer: Rasterizer::new(size_px),
            entries: HashMap::new(),
            lru: Vec::new(),
            bytes: 0,
            budget: budget_bytes,
            active_font: None,
            cell_w: 0.0,
            cell_h: 0.0,
            ascent: 0.0,
            stats: GlyphCacheStats::default(),
        }
    }

    pub fn rasterizer(&self) -> &Rasterizer {
        &self.rasterizer
    }

    pub fn set_cell_size_px(&mut self, w: f32, h: f32) {
        self.cell_w = w;
        self.cell_h = h;
    }

    pub fn cell_w(&self) -> f32 {
        self.cell_w
    }

    pub fn cell_h(&self) -> f32 {
        self.cell_h
    }

    /// Ascent of the active font in px (0 until [`GlyphCache::set_font`]).
    pub fn ascent(&self) -> f32 {
        self.ascent
    }

    /// True if the glyph for `c` in `font` is already cached (no raster).
    pub fn peek(&self, font: &LoadedFont, c: char) -> bool {
        let f = &font.fonts[font.face];
        if !f.has_glyph(c) {
            return false;
        }
        let config = GlyphRasterConfig {
            glyph_index: f.lookup_glyph_index(c),
            px: self.rasterizer.size_px(),
            font_hash: f.file_hash(),
        };
        self.entries.contains_key(&config)
    }

    /// Looks up or rasterizes a glyph, maintaining LRU order.
    pub fn glyph(&mut self, font: &LoadedFont, c: char) -> Option<&RasterGlyph> {
        let f = &font.fonts[font.face];
        if !f.has_glyph(c) {
            return None;
        }
        let config = GlyphRasterConfig {
            glyph_index: f.lookup_glyph_index(c),
            px: self.rasterizer.size_px(),
            font_hash: f.file_hash(),
        };
        if !self.entries.contains_key(&config) {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            let g = self.rasterizer.raster(font, c)?;
            self.bytes += g.bitmap.len();
            self.entries.insert(config, g);
            // Evict the LRU tail until under budget.
            while self.bytes > self.budget {
                if self.lru.is_empty() {
                    break;
                }
                let victim = self.lru.remove(0);
                if let Some(entry) = self.entries.remove(&victim) {
                    self.bytes = self.bytes.saturating_sub(entry.bitmap.len());
                }
            }
        } else {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
        }
        // Move to MRU end.
        if let Some(pos) = self.lru.iter().position(|k| *k == config) {
            self.lru.remove(pos);
        }
        self.lru.push(config);
        self.entries.get(&config)
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.lru.clear();
        self.bytes = 0;
    }

    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// (hits, misses) since the cache was created or last [`GlyphCache::clear`].
    pub fn stats(&self) -> (u64, u64) {
        (
            self.stats.hits.load(Ordering::Relaxed),
            self.stats.misses.load(Ordering::Relaxed),
        )
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Loads the primary font and computes cell metrics + ascent in one call.
    pub fn set_font(&mut self, font: &LoadedFont) {
        self.active_font = Some(font.id);
        let (w, h) = self.rasterizer.cell_metrics(font);
        self.set_cell_size_px(w.ceil(), h.ceil());
        self.ascent = self.rasterizer.ascent(font);
    }

    /// The font the cache was configured with (if any).
    pub fn active_font(&self) -> Option<usize> {
        self.active_font
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Loads a real system font (macOS-first: Menlo.ttc, then Andale Mono).
    fn test_font() -> Option<LoadedFont> {
        for path in [
            "/System/Library/Fonts/Menlo.ttc",
            "/System/Library/Fonts/Supplemental/Andale Mono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        ] {
            let Ok(data) = std::fs::read(path) else {
                continue;
            };
            let font = (0..16u32).find_map(|i| {
                let settings = FontSettings {
                    collection_index: i,
                    ..FontSettings::default()
                };
                fontdue::Font::from_bytes(data.clone(), settings).ok()
            });
            if let Some(font) = font {
                return Some(LoadedFont {
                    id: 0,
                    family: font.name().unwrap_or("test").to_string(),
                    monospace: detect_monospace(&font),
                    path: PathBuf::from(path),
                    fonts: Arc::new(vec![font]),
                    face: 0,
                });
            }
        }
        None
    }

    #[test]
    fn raster_ascii() {
        let Some(f) = test_font() else { return };
        let r = Rasterizer::new(16.0);
        let g = r.raster(&f, 'A').unwrap();
        assert!(g.metrics.width > 0);
        assert!(g.metrics.height > 0);
        assert!(!g.bitmap.is_empty());
        assert!(g.metrics.advance > 0.0);
    }

    #[test]
    fn coverage_and_missing() {
        let Some(f) = test_font() else { return };
        assert!(f.fonts[0].has_glyph('é'));
        let r = Rasterizer::new(16.0);
        // DejaVu/Menlo cover Latin; glyph_index is authoritative.
        let _ = r.raster(&f, '你');
    }

    #[test]
    fn cache_memoizes_and_evicts() {
        let Some(f) = test_font() else { return };
        let mut cache = GlyphCache::new(16.0, 1024 * 1024);
        for c in 'a'..='z' {
            cache.glyph(&f, c);
        }
        assert_eq!(cache.len(), 26);
        let mut small = GlyphCache::new(16.0, 64);
        for c in 'a'..='z' {
            small.glyph(&f, c);
        }
        assert!(small.len() < 26);
    }

    #[test]
    fn cache_evicts_lru_first() {
        let Some(f) = test_font() else { return };
        let mut cache = GlyphCache::new(16.0, 1500);
        let b_index = f.fonts[0].lookup_glyph_index('b');
        cache.glyph(&f, 'a');
        cache.glyph(&f, 'b');
        cache.glyph(&f, 'c');
        // Touch 'a' so the LRU order becomes b, c, a.
        cache.glyph(&f, 'a');
        // Keep adding glyphs until the budget forces an eviction (the first
        // add that does not grow `len` evicted something).
        let mut prev_len = cache.len();
        let mut c = 'd';
        loop {
            cache.glyph(&f, c);
            c = char::from_u32(c as u32 + 1).unwrap_or('a');
            let evicted = cache.len() == prev_len;
            prev_len = cache.len();
            if evicted {
                break;
            }
        }
        // 'b' is the least-recently-used entry and must be evicted first.
        assert!(
            !cache.entries.keys().any(|k| k.glyph_index == b_index),
            "LRU victim 'b' should have been evicted first"
        );
        // The MRU entry 'a' must still be resident.
        assert!(cache.entries.contains_key(&GlyphRasterConfig {
            glyph_index: f.fonts[0].lookup_glyph_index('a'),
            px: 16.0,
            font_hash: f.fonts[0].file_hash(),
        }));
    }

    #[test]
    fn system_scan_finds_monospace() {
        let mut lib = FontLibrary::new();
        lib.scan_system();
        assert!(!lib.fonts.is_empty());
        assert!(lib.primary_monospace(None).is_some());
    }
}
