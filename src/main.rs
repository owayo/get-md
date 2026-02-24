mod progress;

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use headless_chrome::protocol::cdp::Network;
use headless_chrome::{Browser, LaunchOptions};
use url::Url;

use crate::progress::Progress;

/// Fetch a URL in a browser and convert selected elements to Markdown.
/// Uses Chrome/Chromium installed on the system and supports
/// JavaScript-rendered pages.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Target URL to fetch
    url: String,

    /// CSS selectors for elements to convert to Markdown (can be specified multiple times).
    /// If omitted, the entire page (body) is used.
    #[arg(short, long)]
    selector: Vec<String>,

    /// Output file path. If omitted, writes to stdout.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Path to Chrome binary. If omitted, auto-detected from the system.
    #[arg(long)]
    chrome_path: Option<PathBuf>,

    /// Additional wait time in seconds after page load (for JS rendering to complete)
    #[arg(short, long, default_value_t = 2)]
    wait: u64,

    /// Page load timeout in seconds
    #[arg(short, long, default_value_t = 60)]
    timeout: u64,

    /// Show the browser window (for debugging)
    #[arg(long)]
    no_headless: bool,

    /// Disable browser cache (always fetch latest content)
    #[arg(long)]
    no_cache: bool,

    /// Suppress progress output
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut progress = Progress::new(!cli.quiet);

    let selectors = if cli.selector.is_empty() {
        vec!["body".to_string()]
    } else {
        cli.selector
    };

    // Launch browser
    progress.spinner("Launching Chrome...");
    let launch_options = LaunchOptions {
        headless: !cli.no_headless,
        path: cli.chrome_path,
        idle_browser_timeout: idle_browser_timeout(cli.timeout),
        ..LaunchOptions::default()
    };

    let browser = Browser::new(launch_options)
        .context("Failed to launch Chrome. Make sure Chrome is installed on your system")?;

    let tab = browser.new_tab().context("Failed to open new tab")?;
    tab.set_default_timeout(Duration::from_secs(cli.timeout));
    if cli.no_cache {
        tab.call_method(Network::SetCacheDisabled {
            cache_disabled: true,
        })
        .context("Failed to disable browser cache")?;
    }
    progress.finish("Chrome launched");

    // Navigate to page
    progress.spinner(&format!("Loading page: {}", cli.url));
    tab.navigate_to(&cli.url)
        .with_context(|| format!("Failed to navigate to URL: {}", cli.url))?;

    tab.wait_until_navigated().context("Page load timed out")?;

    // Additional wait for JS rendering to complete
    if cli.wait > 0 {
        progress.set_message(&format!("Waiting for JS rendering ({}s)...", cli.wait));
        std::thread::sleep(Duration::from_secs(cli.wait));
    }
    progress.finish("Page loaded");

    // Extract HTML for elements matching the selectors
    progress.spinner("Extracting HTML elements...");
    let mut html_fragments = Vec::new();
    for selector in &selectors {
        progress.set_message(&format!("Extracting selector '{}'...", selector));

        // Get outerHTML of all matching elements
        let js = format!(
            r#"(() => {{
                const els = document.querySelectorAll({selector});
                return Array.from(els).map(el => el.outerHTML).join('\n');
            }})()"#,
            selector = escape_js_string(selector),
        );

        let result = tab
            .evaluate(&js, false)
            .with_context(|| format!("Failed to evaluate selector '{}'", selector))?;

        let html = result
            .value
            .as_ref()
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if html.is_empty() {
            eprintln!("Warning: no elements matched selector '{}'", selector);
        } else {
            html_fragments.push(html);
        }
    }
    progress.finish_and_clear();

    if html_fragments.is_empty() {
        bail!("No elements matched the specified selectors");
    }

    // Convert HTML to Markdown
    progress.spinner("Converting to Markdown...");
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "svg"])
        .options(htmd::options::Options {
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Default::default()
        })
        .build();
    let mut md_parts = Vec::new();
    for html in &html_fragments {
        let md = converter
            .convert(html)
            .context("Failed to convert HTML to Markdown")?;
        md_parts.push(md);
    }

    let markdown = compact_markdown(&md_parts.join("\n\n---\n\n"));
    let markdown = resolve_markdown_urls(&markdown, &cli.url);
    progress.finish("Converted to Markdown");

    // Output
    let mut writer: Box<dyn Write> = match &cli.output {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("Failed to create output directory: {}", parent.display())
                })?;
            }
            let file = File::create(path)
                .with_context(|| format!("Failed to create output file: {}", path.display()))?;
            Box::new(file)
        }
        None => Box::new(io::stdout().lock()),
    };

    writer
        .write_all(markdown.as_bytes())
        .context("Failed to write output")?;

    // Ensure trailing newline for file output
    if cli.output.is_some() && !markdown.ends_with('\n') {
        writer
            .write_all(b"\n")
            .context("Failed to write trailing newline")?;
    }

    // Show completion with URL only after output succeeds.
    progress.complete(&cli.url);

    Ok(())
}

fn idle_browser_timeout(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs.saturating_add(30))
}

/// Escape a CSS selector string as a JavaScript string literal
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\u{2028}' => out.push_str(r"\u2028"),
            '\u{2029}' => out.push_str(r"\u2029"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Compact redundant whitespace in Markdown table rows.
///
/// - Trim padding in table cells
/// - Minimize separator dashes in table rows (preserving alignment `:`)
fn compact_markdown(md: &str) -> String {
    let mut in_fenced_code_block = false;
    let mut fence_char = '\0';
    let mut fence_len = 0usize;

    md.lines()
        .map(|line| {
            let trimmed_start = line.trim_start();
            if let Some((marker, marker_len)) = fence_marker(trimmed_start) {
                if !in_fenced_code_block {
                    in_fenced_code_block = true;
                    fence_char = marker;
                    fence_len = marker_len;
                    return line.to_string();
                }
                if marker == fence_char && marker_len >= fence_len {
                    in_fenced_code_block = false;
                    fence_char = '\0';
                    fence_len = 0;
                    return line.to_string();
                }
            }
            if in_fenced_code_block {
                return line.to_string();
            }

            let trimmed = line.trim();
            if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
                compact_table_row(trimmed)
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }

    let len = line.chars().take_while(|c| *c == marker).count();
    if len >= 3 { Some((marker, len)) } else { None }
}

fn compact_table_row(row: &str) -> String {
    let inner = &row[1..row.len() - 1];
    let cells: Vec<String> = inner
        .split('|')
        .map(|cell| {
            let t = cell.trim();
            if !t.is_empty() && t.chars().all(|c| c == '-' || c == ':') {
                // Separator cell: keep only alignment markers
                let start = if t.starts_with(':') { ":" } else { "" };
                let end = if t.ends_with(':') { ":" } else { "" };
                format!("{start}-{end}")
            } else {
                t.to_string()
            }
        })
        .collect();
    format!("| {} |", cells.join(" | "))
}

/// Resolve relative URLs in Markdown link/image syntax `[text](url)` to absolute
/// using the page URL as the base.
fn resolve_markdown_urls(md: &str, base_url: &str) -> String {
    let base = match Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return md.to_string(),
    };

    let mut result = String::with_capacity(md.len());
    let mut cursor = 0usize;

    while let Some(rel) = md[cursor..].find("](") {
        let open = cursor + rel;
        let inside_start = open + 2;

        result.push_str(&md[cursor..inside_start]);

        let part = &md[inside_start..];
        if let Some(close) = find_link_close_paren(part) {
            let inside = &part[..close];
            let (url, title, use_angle_brackets) = split_link_destination(inside);

            if !url.is_empty() {
                match base.join(url) {
                    Ok(resolved) => {
                        if use_angle_brackets {
                            result.push('<');
                            result.push_str(resolved.as_str());
                            result.push('>');
                        } else {
                            result.push_str(resolved.as_str());
                        }
                    }
                    Err(_) => {
                        if use_angle_brackets {
                            result.push('<');
                            result.push_str(url);
                            result.push('>');
                        } else {
                            result.push_str(url);
                        }
                    }
                }
            } else if use_angle_brackets {
                result.push_str("<>");
            }
            result.push_str(title);
            result.push(')');
            cursor = inside_start + close + 1;
        } else {
            result.push_str(part);
            return result;
        }
    }

    result.push_str(&md[cursor..]);
    result
}

/// Split a Markdown link destination into URL and title.
///
/// Supports:
/// - standard form: `./path "title"`
/// - angle bracket form: `<./path with space> "title"`
fn split_link_destination(inside: &str) -> (&str, &str, bool) {
    if let Some(after_open) = inside.strip_prefix('<')
        && let Some(close) = after_open.find('>')
    {
        let end = close + 1;
        let url = &inside[1..end];
        let title = &inside[(end + 1)..];
        return (url, title, true);
    }

    // In the standard form, the title (if any) starts after the first
    // *unescaped* whitespace.
    let mut backslash_run = 0usize;
    for (i, c) in inside.char_indices() {
        if c == '\\' {
            backslash_run += 1;
            continue;
        }
        let escaped = backslash_run % 2 == 1;
        if c.is_ascii_whitespace() && !escaped {
            return (&inside[..i], &inside[i..], false);
        }
        backslash_run = 0;
    }
    (inside, "", false)
}

/// Find the closing `)` that matches the implicit opening `(` from `](`.
fn find_link_close_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    let mut backslash_run = 0usize;
    let mut title_quote: Option<char> = None;
    let mut saw_dest_non_ws = false;
    let mut saw_sep_ws = false;

    for (i, c) in s.char_indices() {
        let escaped = c != '\\' && backslash_run % 2 == 1;

        if c == '\\' {
            backslash_run += 1;
            continue;
        }

        if let Some(quote) = title_quote {
            if c == quote && !escaped {
                title_quote = None;
            }
            backslash_run = 0;
            continue;
        }

        if depth == 1 {
            if c.is_ascii_whitespace() {
                if saw_dest_non_ws {
                    saw_sep_ws = true;
                }
            } else if saw_sep_ws && (c == '"' || c == '\'') {
                title_quote = Some(c);
                backslash_run = 0;
                continue;
            } else {
                saw_dest_non_ws = true;
                saw_sep_ws = false;
            }
        }

        match c {
            '(' if !escaped => depth += 1,
            ')' if !escaped => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }

        backslash_run = 0;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_simple_selector() {
        assert_eq!(escape_js_string("body"), r#""body""#);
    }

    #[test]
    fn escape_selector_with_quotes() {
        assert_eq!(escape_js_string(r#"a[href="x"]"#), r#""a[href=\"x\"]""#);
    }

    #[test]
    fn escape_selector_with_backslash() {
        assert_eq!(escape_js_string(r"div\.class"), r#""div\\.class""#);
    }

    #[test]
    fn escape_selector_with_newline() {
        assert_eq!(escape_js_string("a\nb"), r#""a\nb""#);
    }

    #[test]
    fn escape_selector_with_carriage_return() {
        assert_eq!(escape_js_string("a\rb"), r#""a\rb""#);
    }

    #[test]
    fn escape_empty_string() {
        assert_eq!(escape_js_string(""), r#""""#);
    }

    #[test]
    fn escape_complex_css_selector() {
        assert_eq!(
            escape_js_string("div > .content p:nth-child(2)"),
            r#""div > .content p:nth-child(2)""#,
        );
    }

    #[test]
    fn cli_default_values() {
        let cli = Cli::try_parse_from(["get-md", "https://example.com"]).unwrap();
        assert_eq!(cli.url, "https://example.com");
        assert!(cli.selector.is_empty());
        assert!(cli.output.is_none());
        assert!(cli.chrome_path.is_none());
        assert_eq!(cli.wait, 2);
        assert_eq!(cli.timeout, 60);
        assert!(!cli.no_headless);
        assert!(!cli.quiet);
    }

    #[test]
    fn cli_all_options() {
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "-s",
            "article",
            "-s",
            ".content",
            "-o",
            "out.md",
            "-w",
            "5",
            "-t",
            "60",
            "--no-headless",
            "--no-cache",
            "-q",
        ])
        .unwrap();
        assert_eq!(cli.url, "https://example.com");
        assert_eq!(cli.selector, vec!["article", ".content"]);
        assert_eq!(cli.output.unwrap().to_str().unwrap(), "out.md");
        assert_eq!(cli.wait, 5);
        assert_eq!(cli.timeout, 60);
        assert!(cli.no_headless);
        assert!(cli.no_cache);
        assert!(cli.quiet);
    }

    #[test]
    fn cli_missing_url_fails() {
        assert!(Cli::try_parse_from(["get-md"]).is_err());
    }

    #[test]
    fn cli_single_selector() {
        let cli = Cli::try_parse_from(["get-md", "https://example.com", "-s", "main"]).unwrap();
        assert_eq!(cli.selector, vec!["main"]);
    }

    #[test]
    fn cli_chrome_path_option() {
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "--chrome-path",
            "/usr/bin/chromium",
        ])
        .unwrap();
        assert_eq!(
            cli.chrome_path.unwrap().to_str().unwrap(),
            "/usr/bin/chromium"
        );
    }

    #[test]
    fn idle_browser_timeout_adds_buffer() {
        assert_eq!(idle_browser_timeout(60), Duration::from_secs(90));
    }

    #[test]
    fn idle_browser_timeout_saturates_on_overflow() {
        assert_eq!(
            idle_browser_timeout(u64::MAX),
            Duration::from_secs(u64::MAX),
        );
    }

    #[test]
    fn escape_unicode_selector() {
        assert_eq!(escape_js_string(".日本語"), r#"".日本語""#);
    }

    #[test]
    fn escape_tab_character() {
        assert_eq!(escape_js_string("a\tb"), "\"a\tb\"");
    }

    #[test]
    fn escape_single_quotes_passthrough() {
        assert_eq!(escape_js_string("div[data-x='y']"), r#""div[data-x='y']""#);
    }

    // compact_markdown tests

    #[test]
    fn compact_table_cell_padding() {
        assert_eq!(compact_markdown("| aaaa           |"), "| aaaa |",);
        assert_eq!(
            compact_markdown("| col1           | col2       |"),
            "| col1 | col2 |",
        );
    }

    #[test]
    fn compact_table_separator_dashes() {
        assert_eq!(compact_markdown("| -------------- |"), "| - |",);
        assert_eq!(
            compact_markdown("| -------------- | -------------- |"),
            "| - | - |",
        );
    }

    #[test]
    fn compact_table_separator_preserves_alignment() {
        assert_eq!(compact_markdown("| :--- |"), "| :- |");
        assert_eq!(compact_markdown("| ---: |"), "| -: |");
        assert_eq!(compact_markdown("| :---: |"), "| :-: |");
        assert_eq!(
            compact_markdown("| :-------------- | --------------: | :--------------: |"),
            "| :- | -: | :-: |",
        );
    }

    #[test]
    fn compact_table_already_compact() {
        assert_eq!(compact_markdown("| a | b |"), "| a | b |");
        assert_eq!(compact_markdown("| - | - |"), "| - | - |");
    }

    #[test]
    fn compact_multiline_mixed() {
        let input = "\
# Title

* First item
* Second item

| Name           | Value          |
| -------------- | -------------- |
| foo            | bar            |";

        let expected = "\
# Title

* First item
* Second item

| Name | Value |
| - | - |
| foo | bar |";

        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_preserves_fenced_code_block() {
        let input = "\
```md
| Name           | Value          |
| -------------- | -------------- |
| foo            | bar            |
```";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_preserves_tilde_fenced_code_block() {
        let input = "\
~~~text
| keep           | spacing        |
~~~";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_preserves_non_table_lines() {
        assert_eq!(compact_markdown("---"), "---");
        assert_eq!(compact_markdown("- single space"), "- single space");
        assert_eq!(compact_markdown("Hello world"), "Hello world");
        assert_eq!(compact_markdown(""), "");
    }

    // resolve_markdown_urls tests

    const BASE: &str = "https://example.com/docs/en/page.md";

    #[test]
    fn resolve_relative_link() {
        assert_eq!(
            resolve_markdown_urls("[link](./other.md)", BASE),
            "[link](https://example.com/docs/en/other.md)",
        );
    }

    #[test]
    fn resolve_root_relative_link() {
        assert_eq!(
            resolve_markdown_urls("[link](/root/path)", BASE),
            "[link](https://example.com/root/path)",
        );
    }

    #[test]
    fn resolve_parent_relative_link() {
        assert_eq!(
            resolve_markdown_urls("[link](../sibling.md)", BASE),
            "[link](https://example.com/docs/sibling.md)",
        );
    }

    #[test]
    fn resolve_absolute_url_unchanged() {
        assert_eq!(
            resolve_markdown_urls("[link](https://other.com/page)", BASE),
            "[link](https://other.com/page)",
        );
    }

    #[test]
    fn resolve_fragment_only() {
        assert_eq!(
            resolve_markdown_urls("[link](#section)", BASE),
            "[link](https://example.com/docs/en/page.md#section)",
        );
    }

    #[test]
    fn resolve_image_url() {
        assert_eq!(
            resolve_markdown_urls("![alt](./img.png)", BASE),
            "![alt](https://example.com/docs/en/img.png)",
        );
    }

    #[test]
    fn resolve_link_with_title() {
        assert_eq!(
            resolve_markdown_urls(r#"[link](./page "Title")"#, BASE),
            r#"[link](https://example.com/docs/en/page "Title")"#,
        );
    }

    #[test]
    fn resolve_link_with_tab_before_title() {
        assert_eq!(
            resolve_markdown_urls("[link](./page\t\"Title\")", BASE),
            "[link](https://example.com/docs/en/page\t\"Title\")",
        );
    }

    #[test]
    fn resolve_url_with_apostrophe_in_path() {
        assert_eq!(
            resolve_markdown_urls("[link](./it's.md)", BASE),
            "[link](https://example.com/docs/en/it's.md)",
        );
    }

    #[test]
    fn resolve_multiple_links() {
        let input = "[a](./one) and [b](../two) and [c](https://abs.com/page)";
        let expected = "[a](https://example.com/docs/en/one) and [b](https://example.com/docs/two) and [c](https://abs.com/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_no_links_unchanged() {
        assert_eq!(resolve_markdown_urls("plain text", BASE), "plain text",);
    }

    #[test]
    fn resolve_empty_url_unchanged() {
        assert_eq!(resolve_markdown_urls("[link]()", BASE), "[link]()",);
    }

    #[test]
    fn resolve_invalid_base_url_unchanged() {
        assert_eq!(
            resolve_markdown_urls("[link](./path)", "not a url"),
            "[link](./path)",
        );
    }

    #[test]
    fn resolve_nested_parens_in_url() {
        assert_eq!(
            resolve_markdown_urls("[wiki](/wiki/Rust_(language))", BASE),
            "[wiki](https://example.com/wiki/Rust_(language))",
        );
    }

    // find_link_close_paren direct tests

    #[test]
    fn find_close_paren_simple() {
        assert_eq!(find_link_close_paren("url)"), Some(3));
    }

    #[test]
    fn find_close_paren_nested() {
        assert_eq!(find_link_close_paren("wiki/Rust_(lang))"), Some(16));
    }

    #[test]
    fn find_close_paren_no_close() {
        assert_eq!(find_link_close_paren("no close paren"), None);
    }

    #[test]
    fn find_close_paren_empty() {
        assert_eq!(find_link_close_paren(")"), Some(0));
    }

    #[test]
    fn find_close_paren_deeply_nested() {
        assert_eq!(find_link_close_paren("a(b(c))d)"), Some(8));
    }

    #[test]
    fn find_close_paren_ignores_escaped_close() {
        assert_eq!(find_link_close_paren(r"foo\)bar)"), Some(8));
    }

    #[test]
    fn find_close_paren_ignores_escaped_open() {
        assert_eq!(find_link_close_paren(r"foo\(bar)"), Some(8));
    }

    // compact_table_row edge cases

    #[test]
    fn compact_table_single_cell() {
        assert_eq!(compact_markdown("| only |"), "| only |");
    }

    #[test]
    fn compact_table_empty_cells() {
        assert_eq!(compact_markdown("|  |  |"), "|  |  |");
    }

    #[test]
    fn compact_markdown_empty_input() {
        assert_eq!(compact_markdown(""), "");
    }

    #[test]
    fn compact_markdown_only_newlines() {
        // lines() drops trailing empty strings, so "\n\n\n" (4 lines, last empty) -> "\n\n"
        assert_eq!(compact_markdown("\n\n\n"), "\n\n");
    }

    // resolve_markdown_urls additional edge cases

    #[test]
    fn resolve_url_with_query_string() {
        assert_eq!(
            resolve_markdown_urls("[link](./page?q=test&a=1)", BASE),
            "[link](https://example.com/docs/en/page?q=test&a=1)",
        );
    }

    #[test]
    fn resolve_url_with_fragment_and_query() {
        assert_eq!(
            resolve_markdown_urls("[link](./page?q=1#sec)", BASE),
            "[link](https://example.com/docs/en/page?q=1#sec)",
        );
    }

    #[test]
    fn resolve_protocol_relative_url() {
        assert_eq!(
            resolve_markdown_urls("[link](//cdn.example.com/img.png)", BASE),
            "[link](https://cdn.example.com/img.png)",
        );
    }

    #[test]
    fn resolve_data_url_unchanged() {
        let input = "[img](data:image/png;base64,ABC)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_mailto_link_unchanged() {
        let input = "[email](mailto:test@example.com)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_angle_bracket_url_with_space() {
        assert_eq!(
            resolve_markdown_urls("[doc](<./my file.md>)", BASE),
            "[doc](<https://example.com/docs/en/my%20file.md>)",
        );
    }

    #[test]
    fn resolve_angle_bracket_url_with_title() {
        assert_eq!(
            resolve_markdown_urls(r#"[doc](<./my file.md> "Title")"#, BASE),
            r#"[doc](<https://example.com/docs/en/my%20file.md> "Title")"#,
        );
    }

    #[test]
    fn resolve_angle_bracket_absolute_url_unchanged_except_wrapper() {
        assert_eq!(
            resolve_markdown_urls("[doc](<https://other.com/path with space>)", BASE),
            "[doc](<https://other.com/path%20with%20space>)",
        );
    }

    #[test]
    fn resolve_adjacent_links() {
        let input = "[a](./x)[b](./y)";
        let expected = "[a](https://example.com/docs/en/x)[b](https://example.com/docs/en/y)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_title_containing_link_marker() {
        let input = r#"[a](./one "literal ]( marker")[b](./two)"#;
        let expected = r#"[a](https://example.com/docs/en/one "literal ]( marker")[b](https://example.com/docs/en/two)"#;
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn find_close_paren_ignores_paren_in_quoted_title() {
        assert_eq!(
            find_link_close_paren(r#"./one "title ) marker")"#),
            Some(22),
        );
    }

    #[test]
    fn split_link_destination_standard_with_title() {
        assert_eq!(
            split_link_destination(r#"./page "Title""#),
            ("./page", r#" "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_standard_with_escaped_space() {
        assert_eq!(
            split_link_destination(r#"./my\ file.md "Title""#),
            (r#"./my\ file.md"#, r#" "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_standard_with_escaped_space_without_title() {
        assert_eq!(
            split_link_destination(r#"./my\ file.md"#),
            (r#"./my\ file.md"#, "", false),
        );
    }

    #[test]
    fn split_link_destination_standard_with_even_backslashes_before_space() {
        assert_eq!(
            split_link_destination(r#"./path\\ "Title""#),
            (r#"./path\\"#, r#" "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_angle_bracket_with_title() {
        assert_eq!(
            split_link_destination(r#"<./my file.md> "Title""#),
            ("./my file.md", r#" "Title""#, true),
        );
    }

    // escape_js_string additional edge cases

    #[test]
    fn escape_mixed_special_chars() {
        assert_eq!(escape_js_string("a\"b\\c\nd\re"), r#""a\"b\\c\nd\re""#,);
    }

    #[test]
    fn escape_only_special_chars() {
        assert_eq!(escape_js_string("\"\\"), r#""\"\\""#);
    }

    #[test]
    fn escape_js_line_separator_chars() {
        assert_eq!(
            escape_js_string("a\u{2028}b\u{2029}c"),
            r#""a\u2028b\u2029c""#
        );
    }
}
