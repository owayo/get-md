use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// 取得・変換処理の進捗表示を管理する
pub struct Progress {
    enabled: bool,
    bar: Option<ProgressBar>,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, bar: None }
    }

    /// メッセージ付きスピナーを表示する
    pub fn spinner(&mut self, message: &str) {
        if !self.enabled {
            return;
        }

        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                .template("{spinner:.cyan} {msg}")
                .expect("Invalid template"),
        );
        spinner.set_message(message.to_string());
        spinner.enable_steady_tick(Duration::from_millis(80));
        self.bar = Some(spinner);
    }

    /// 現在のスピナー/バーのメッセージを更新する
    pub fn set_message(&self, message: &str) {
        if let Some(ref bar) = self.bar {
            bar.set_message(message.to_string());
        }
    }

    /// 現在の進捗バーをメッセージ付きで完了させる
    pub fn finish(&mut self, message: &str) {
        if let Some(ref bar) = self.bar {
            bar.finish_with_message(message.to_string());
        }
        self.bar = None;
    }

    /// 緑色チェック付きの完了メッセージを表示する
    pub fn complete(&self, message: &str) {
        if !self.enabled {
            return;
        }
        let bar = ProgressBar::new_spinner();
        bar.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("✔✔")
                .template("{spinner:.green} {msg}")
                .expect("Invalid template"),
        );
        bar.finish_with_message(message.to_string());
    }

    /// 現在の進捗バーを完了して消去する
    pub fn finish_and_clear(&mut self) {
        if let Some(ref bar) = self.bar {
            bar.finish_and_clear();
        }
        self.bar = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_progress_does_not_create_spinner() {
        let mut p = Progress::new(false);
        p.spinner("test");
        assert!(p.bar.is_none());
    }

    #[test]
    fn enabled_progress_creates_spinner() {
        let mut p = Progress::new(true);
        p.spinner("test");
        assert!(p.bar.is_some());
        p.finish_and_clear();
    }

    #[test]
    fn finish_clears_bar() {
        let mut p = Progress::new(true);
        p.spinner("loading");
        p.finish("done");
        assert!(p.bar.is_none());
    }

    #[test]
    fn finish_and_clear_clears_bar() {
        let mut p = Progress::new(true);
        p.spinner("loading");
        p.finish_and_clear();
        assert!(p.bar.is_none());
    }

    #[test]
    fn set_message_on_disabled_does_not_panic() {
        let p = Progress::new(false);
        p.set_message("should not panic");
    }

    #[test]
    fn finish_without_spinner_does_not_panic() {
        let mut p = Progress::new(true);
        p.finish("no spinner");
        assert!(p.bar.is_none());
    }

    #[test]
    fn finish_and_clear_without_spinner_does_not_panic() {
        let mut p = Progress::new(false);
        p.finish_and_clear();
        assert!(p.bar.is_none());
    }

    #[test]
    fn complete_on_disabled_does_not_panic() {
        let p = Progress::new(false);
        p.complete("done");
    }

    #[test]
    fn complete_on_enabled_does_not_panic() {
        let p = Progress::new(true);
        p.complete("https://example.com");
    }

    #[test]
    fn set_message_with_active_spinner() {
        let mut p = Progress::new(true);
        p.spinner("loading");
        p.set_message("updated message");
        p.finish_and_clear();
    }

    #[test]
    fn multiple_spinner_cycles() {
        let mut p = Progress::new(true);
        p.spinner("first");
        p.finish("first done");
        assert!(p.bar.is_none());
        p.spinner("second");
        p.finish("second done");
        assert!(p.bar.is_none());
    }
}
