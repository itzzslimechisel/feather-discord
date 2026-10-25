fn main() {
    tauri::Builder::default()
        .on_page_load(|_w, p| { if matches!(p.event(), tauri::webview::PageLoadEvent::Finished) { println!("LOADED {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()); } })
        .run(tauri::generate_context!())
        .expect("run");
}
