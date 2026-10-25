//! Optional system fonts for bold text and wider glyph coverage. egui's bundled fonts have no
//! bold face, so when a known system bold font exists it is registered as the "bold" family.
//! Nothing here runs before the first frame.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use std::sync::Arc;

const BOLD: &[&str] = &[
    "C:\\Windows\\Fonts\\segoeuib.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/truetype/ubuntu/Ubuntu-B.ttf",
    "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
    "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans-Bold.ttf",
    "/Library/Fonts/Arial Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
];

const FALLBACK: &[&str] = &[
    "C:\\Windows\\Fonts\\seguisym.ttf",
    "C:\\Windows\\Fonts\\msyh.ttc",
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/System/Library/Fonts/PingFang.ttc",
];

/// Returns true if a bold face was installed.
pub fn install_system_fonts(ctx: &egui::Context, wide: bool) -> bool {
    let mut defs = FontDefinitions::default();
    let mut has_bold = false;
    if let Some((path, data)) = BOLD.iter().find_map(|p| std::fs::read(p).ok().map(|d| (*p, d))) {
        log::debug!("bold font: {path}");
        defs.font_data.insert("sys-bold".into(), Arc::new(FontData::from_owned(data)));
        defs.families.insert(FontFamily::Name("bold".into()), vec!["sys-bold".into()]);
        has_bold = true;
    }
    for (i, p) in FALLBACK.iter().enumerate().filter(|_| wide) {
        if let Ok(data) = std::fs::read(p) {
            // Files over 40 MB would cost noticeable memory; skip those.
            if data.len() > 40 << 20 {
                continue;
            }
            let name = format!("sys-fallback-{i}");
            defs.font_data.insert(name.clone(), Arc::new(FontData::from_owned(data)));
            for fam in [FontFamily::Proportional, FontFamily::Monospace] {
                defs.families.entry(fam).or_default().push(name.clone());
            }
            break;
        }
    }
    if !has_bold {
        defs.families.insert(FontFamily::Name("bold".into()), defs.families[&FontFamily::Proportional].clone());
    }
    ctx.set_fonts(defs);
    has_bold
}
