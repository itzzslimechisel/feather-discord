//! Desktop notifications for mentions and direct messages. Message text is passed as plain
//! text and truncated; notification servers that interpret markup receive it escaped.

use std::time::{Duration, Instant};

pub struct Notifier {
    pub enabled: bool,
    pub all_dms: bool,
    last: Option<Instant>,
    suppressed: u32,
}

impl Notifier {
    pub fn new(enabled: bool, all_dms: bool) -> Notifier {
        Notifier { enabled, all_dms, last: None, suppressed: 0 }
    }

    /// Shows a notification unless one was shown in the last 3 s (bursts are summarised).
    pub fn show(&mut self, title: &str, body: &str) {
        if !self.enabled || std::env::var_os("FEATHER_NO_NOTIFY").is_some() {
            return;
        }
        let now = Instant::now();
        if self.last.is_some_and(|t| now.duration_since(t) < Duration::from_secs(3)) {
            self.suppressed += 1;
            return;
        }
        self.last = Some(now);
        let mut body: String = body.chars().filter(|c| !c.is_control() || *c == '\n').take(160).collect();
        if self.suppressed > 0 {
            body.push_str(&format!("\n(+{} more)", self.suppressed));
            self.suppressed = 0;
        }
        let body = escape(&body);
        let title: String = escape(&title.chars().take(80).collect::<String>());
        // Showing a notification can block on D-Bus; never do it on the async thread.
        std::thread::Builder::new()
            .name("feather-notify".into())
            .spawn(move || {
                if let Err(e) = notify_rust::Notification::new().appname("Feather").summary(&title).body(&body).show() {
                    log::info!("notification failed: {e}");
                }
            })
            .ok();
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
