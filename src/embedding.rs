//! In-process page extraction. Chrome is still used as the rendering engine.
#![deny(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use anyhow::{Result, bail};
use clap::Parser;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use url::Url;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            bail!("Page extraction cancelled");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FetchOptions {
    pub url: String,
    pub selectors: Vec<String>,
    pub chrome_path: Option<PathBuf>,
    pub timeout: Duration,
    pub wait: Duration,
}
impl FetchOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            selectors: Vec::new(),
            chrome_path: None,
            timeout: Duration::from_secs(30),
            wait: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Document {
    pub markdown: String,
    pub url: String,
    pub final_url: String,
    pub title: Option<String>,
}

/// Fetches without printing or writing an output file. No host argv is consumed.
/// Cancellation is checked between CDP operations and during the render wait.
/// A blocking navigation is bounded by `timeout`; the owned browser is then dropped.
pub fn fetch(options: FetchOptions, cancellation: &Cancellation) -> Result<Document> {
    cancellation.check()?;
    let url = Url::parse(options.url.trim())?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("An HTTP or HTTPS URL without embedded credentials is required");
    }
    if options.timeout.is_zero()
        || options.timeout > Duration::from_secs(120)
        || options.wait > Duration::from_secs(30)
    {
        bail!("Invalid page extraction time limits");
    }
    let mut cli = crate::Cli::try_parse_from(["get-md", url.as_str()])?;
    cli.chrome_path = options.chrome_path;
    cli.timeout = options.timeout.as_secs().max(1);
    cli.wait = options.wait.as_secs();
    let mut progress = crate::Progress::new(false);
    let selectors = crate::prepare_selectors(&options.selectors);
    let page = crate::launch_browser(&cli, &mut progress)?;
    cancellation.check()?;
    crate::load_page(&page.tab, url.as_str(), 0, &mut progress)?;
    cancellation.check()?;
    crate::validate_http_status(&page.main_response_status, url.as_str())?;
    let until = Instant::now() + options.wait;
    while Instant::now() < until {
        cancellation.check()?;
        std::thread::sleep(
            Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())),
        );
    }
    let extracted = crate::extract_html_fragments(&page.tab, &selectors, false, &mut progress)?;
    cancellation.check()?;
    let metadata = crate::read_source_metadata(&page.tab, url.as_str(), &selectors, Vec::new())?;
    let markdown = crate::convert_html_fragments(
        &page.tab,
        &extracted.fragments,
        url.as_str(),
        true,
        &mut crate::InvisibleCount::default(),
        &mut progress,
    )?;
    cancellation.check()?;
    // Keep ownership of Browser until every operation has finished.
    let _ = &page.browser;
    Ok(Document {
        markdown,
        url: url.to_string(),
        final_url: page.tab.get_url(),
        title: metadata.title,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unsafe_or_malformed_urls_are_rejected_before_browser_launch() {
        for url in [
            "file:///tmp/page",
            "javascript:alert(1)",
            "data:text/html,x",
            "https://user:pass@example.com",
            "not a URL",
        ] {
            assert!(fetch(FetchOptions::new(url), &Cancellation::default()).is_err());
        }
    }
    #[test]
    fn cancelled_request_never_launches_browser() {
        let cancellation = Cancellation::default();
        cancellation.cancel();
        assert!(
            fetch(FetchOptions::new("https://example.com"), &cancellation)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }
}
