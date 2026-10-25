//! Bounded texture cache. Images are requested from the engine on first use, decoded off the UI
//! thread at display size, uploaded when they arrive and evicted least-recently-used once the
//! byte budget is exceeded. Evicted entries are simply requested again if they scroll back in.

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use feather_core::engine::Command;
use feather_core::images::DecodedImage;
use std::collections::HashMap;

pub enum Slot {
    Loading,
    Failed,
    Ready { tex: TextureHandle, bytes: usize, size: [u32; 2] },
}

pub struct ImageCache {
    map: HashMap<String, (Slot, u64)>,
    tick: u64,
    bytes: usize,
    pub budget: usize,
    pub enabled: bool,
}

pub enum Lookup<'a> {
    Ready(&'a TextureHandle, [u32; 2]),
    Pending,
    Failed,
}

impl ImageCache {
    pub fn new(budget: usize) -> ImageCache {
        ImageCache { map: HashMap::new(), tick: 0, bytes: 0, budget, enabled: true }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns the texture if present; otherwise queues a fetch (once) via `request`.
    pub fn get(&mut self, url: &str, max_w: u32, max_h: u32, request: &mut dyn FnMut(Command)) -> Lookup<'_> {
        self.tick += 1;
        let tick = self.tick;
        if !self.map.contains_key(url) {
            if !self.enabled {
                return Lookup::Pending;
            }
            self.map.insert(url.to_string(), (Slot::Loading, tick));
            request(Command::FetchImage { url: url.to_string(), max_w, max_h });
        }
        let e = self.map.get_mut(url).unwrap();
        e.1 = tick;
        match &e.0 {
            Slot::Loading => Lookup::Pending,
            Slot::Failed => Lookup::Failed,
            Slot::Ready { tex, size, .. } => Lookup::Ready(tex, *size),
        }
    }

    pub fn insert(&mut self, ctx: &egui::Context, url: String, result: Result<DecodedImage, String>) {
        self.tick += 1;
        let slot = match result {
            Ok(img) => {
                let ci = ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
                let tex = ctx.load_texture(&url, ci, TextureOptions::LINEAR);
                let bytes = img.rgba.len();
                self.bytes += bytes;
                Slot::Ready { tex, bytes, size: [img.width, img.height] }
            }
            Err(e) => {
                log::debug!("image failed: {e}");
                Slot::Failed
            }
        };
        if let Some((Slot::Ready { bytes, .. }, _)) = self.map.get(&url) {
            self.bytes -= bytes;
        }
        self.map.insert(url, (slot, self.tick));
        self.evict();
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(_, (s, _))| matches!(s, Slot::Ready { .. }))
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| k.clone());
            let Some(k) = victim else { break };
            if let Some((Slot::Ready { bytes, .. }, _)) = self.map.remove(&k) {
                self.bytes -= bytes;
            }
        }
        // Failed entries are cheap but unbounded in count: cap them.
        if self.map.len() > 4096 {
            self.map.retain(|_, (s, _)| matches!(s, Slot::Ready { .. }));
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.bytes = 0;
    }
}
