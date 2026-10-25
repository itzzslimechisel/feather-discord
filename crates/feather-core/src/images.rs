//! Remote image loading: allow-listed hosts only, no credentials, size-capped downloads and
//! decoding off the async thread, downscaled to the size it will be displayed at.

use std::sync::Arc;
use std::time::Duration;

pub const MAX_DOWNLOAD: usize = 16 << 20;
pub const MAX_DIMENSION: u32 = 8192;

#[derive(Clone)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// RGBA8, premultiplied = false.
    pub rgba: Arc<Vec<u8>>,
}

impl std::fmt::Debug for DecodedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DecodedImage({}x{})", self.width, self.height)
    }
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(30))
        .build()
        .expect("image client")
}

pub async fn fetch(http: &reqwest::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut resp = http.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    if resp.content_length().is_some_and(|l| l as usize > MAX_DOWNLOAD) {
        return Err("image too large".into());
    }
    let mut buf = Vec::with_capacity(resp.content_length().unwrap_or(32 * 1024).min(MAX_DOWNLOAD as u64) as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        if buf.len() + chunk.len() > MAX_DOWNLOAD {
            return Err("image too large".into());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Decodes (first frame only for animations) and downscales to fit `max_w` x `max_h`.
pub fn decode(bytes: &[u8], max_w: u32, max_h: u32) -> Result<DecodedImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(256 << 20);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let (max_w, max_h) = (max_w.max(1), max_h.max(1));
    let img = if w > max_w || h > max_h {
        img.resize(max_w, max_h, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let rgba = img.into_rgba8();
    Ok(DecodedImage { width: rgba.width(), height: rgba.height(), rgba: Arc::new(rgba.into_raw()) })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_and_downscale() {
        let img = image::RgbaImage::from_pixel(400, 200, image::Rgba([1, 2, 3, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let d = decode(&png, 100, 100).unwrap();
        assert_eq!((d.width, d.height), (100, 50));
        assert_eq!(d.rgba.len(), 100 * 50 * 4);
        assert!(decode(b"not an image", 10, 10).is_err());
    }
}
