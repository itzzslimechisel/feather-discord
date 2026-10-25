fn main() -> eframe::Result {
    let opts = eframe::NativeOptions { viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]), ..Default::default() };
    let mut first = true;
    eframe::run_simple_native("rb", opts, move |ctx, _| {
        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            let h = ui.text_style_height(&eframe::egui::TextStyle::Body) + 8.0;
            eframe::egui::ScrollArea::vertical().show_rows(ui, h, 500, |ui, range| {
                for i in range { ui.horizontal(|ui| { ui.strong(format!("user{}", i % 17)); ui.label(format!("message number {i} with some ordinary chat text to render in the list")); }); }
            });
        });
        if first { first = false; println!("PAINTED {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()); }
    })
}
