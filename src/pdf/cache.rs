use super::worker::PaperTheme;
use lru::LruCache;
use std::num::NonZeroUsize;

#[derive(Clone)]
pub struct CachedRender {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub is_preview: bool,
}

pub struct PageCache {
    cache: LruCache<(i32, u32, i32, u8), CachedRender>,
}

impl PageCache {
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(12).unwrap());
        Self {
            cache: LruCache::new(cap),
        }
    }

    /// Buckets zoom level as integer percentage (e.g. 1.0 -> 100, 1.25 -> 125)
    pub fn zoom_bucket(zoom: f32) -> u32 {
        (zoom * 100.0).round().max(1.0) as u32
    }

    pub fn get(&mut self, page: i32, zoom: f32, rotation: i32, theme: PaperTheme) -> Option<&CachedRender> {
        self.cache.get(&(page, Self::zoom_bucket(zoom), rotation, theme as u8))
    }

    pub fn get_mut(&mut self, page: i32, zoom: f32, rotation: i32, theme: PaperTheme) -> Option<&mut CachedRender> {
        self.cache.get_mut(&(page, Self::zoom_bucket(zoom), rotation, theme as u8))
    }

    pub fn put(&mut self, page: i32, zoom: f32, rotation: i32, theme: PaperTheme, entry: CachedRender) {
        self.cache.put((page, Self::zoom_bucket(zoom), rotation, theme as u8), entry);
    }

    pub fn remove_page(&mut self, page: i32) {
        let keys_to_remove: Vec<_> = self
            .cache
            .iter()
            .filter(|((p, _, _, _), _)| *p == page)
            .map(|(k, _)| *k)
            .collect();
        for k in keys_to_remove {
            self.cache.pop(&k);
        }
    }

    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.cache.clear();
    }
}
