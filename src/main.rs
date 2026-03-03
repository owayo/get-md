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

/// ブラウザで URL を取得し、指定要素を Markdown に変換する。
/// システムにインストールされた Chrome/Chromium を利用し、
/// JavaScript で描画されるページにも対応する。
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// 取得対象の URL
    url: String,

    /// Markdown 変換対象の CSS セレクタ（複数指定可）。
    /// 省略時はページ全体（body）を対象にする。
    #[arg(short, long)]
    selector: Vec<String>,

    /// 出力ファイルパス。省略時は標準出力へ書き込む。
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Chrome バイナリのパス。省略時はシステムから自動検出する。
    #[arg(long)]
    chrome_path: Option<PathBuf>,

    /// ページ読み込み後の追加待機時間（秒、JS 描画完了待ち）
    #[arg(short, long, default_value_t = 2)]
    wait: u64,

    /// ページ読み込みタイムアウト（秒）
    #[arg(short, long, default_value_t = 60)]
    timeout: u64,

    /// ブラウザウィンドウを表示する（デバッグ用）
    #[arg(long)]
    no_headless: bool,

    /// ブラウザキャッシュを無効化する（常に最新コンテンツを取得）
    #[arg(long)]
    no_cache: bool,

    /// 進捗表示を抑制する
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

    // ブラウザを起動する
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

    // ページへ遷移する
    progress.spinner(&format!("Loading page: {}", cli.url));
    tab.navigate_to(&cli.url)
        .with_context(|| format!("Failed to navigate to URL: {}", cli.url))?;

    tab.wait_until_navigated().context("Page load timed out")?;

    // JS 描画完了を待つための追加待機
    if cli.wait > 0 {
        progress.set_message(&format!("Waiting for JS rendering ({}s)...", cli.wait));
        std::thread::sleep(Duration::from_secs(cli.wait));
    }
    progress.finish("Page loaded");

    // セレクタに一致した要素の HTML を抽出する
    progress.spinner("Extracting HTML elements...");
    let mut html_fragments = Vec::new();
    for selector in &selectors {
        progress.set_message(&format!("Extracting selector '{}'...", selector));

        // 一致した全要素の outerHTML を取得する
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

    // HTML を Markdown に変換する
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

    // 出力内容を確定する（末尾改行を保証）
    let output_bytes = if cli.output.is_some() && !markdown.ends_with('\n') {
        format!("{markdown}\n")
    } else {
        markdown
    };

    // 出力
    let old_content = cli.output.as_ref().and_then(|p| std::fs::read(p).ok());
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
        .write_all(output_bytes.as_bytes())
        .context("Failed to write output")?;

    // 出力成功後にのみ URL 付きの完了表示を行う
    match &cli.output {
        Some(path) => {
            let status = match &old_content {
                None => "created",
                Some(old) if old != output_bytes.as_bytes() => "updated",
                _ => "unchanged",
            };
            progress.complete(&format!("{} → {} ({})", cli.url, path.display(), status));
        }
        None => progress.complete(&cli.url),
    }

    Ok(())
}

fn idle_browser_timeout(timeout_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs.saturating_add(30))
}

/// CSS セレクタ文字列を JavaScript 文字列リテラルとしてエスケープする
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

/// Markdown テーブル行の余分な空白を圧縮する。
///
/// - セルの前後余白を削る
/// - セパレータ行のダッシュを最小化する（配置指定 `:` は保持）
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
    let cells: Vec<String> = split_unescaped_table_cells(inner)
        .into_iter()
        .map(|cell| {
            let t = cell.trim();
            if !t.is_empty() && t.chars().all(|c| c == '-' || c == ':') {
                // セパレータセルは配置指定だけ残す
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

fn split_unescaped_table_cells(inner: &str) -> Vec<&str> {
    let mut cells = Vec::new();
    let mut start = 0usize;
    let mut backslash_run = 0usize;

    for (i, c) in inner.char_indices() {
        if c == '\\' {
            backslash_run += 1;
            continue;
        }

        let escaped = backslash_run % 2 == 1;
        if c == '|' && !escaped {
            cells.push(&inner[start..i]);
            start = i + 1;
        }

        backslash_run = 0;
    }

    cells.push(&inner[start..]);
    cells
}

/// Markdown のリンク/画像構文 `[text](url)` に含まれる相対 URL を
/// ページ URL を基準に絶対 URL へ解決する。
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

/// Markdown のリンク先を URL とタイトルに分割する。
///
/// 対応形式:
/// - 標準形式: `./path "title"`
/// - 山括弧形式: `<./path with space> "title"`
fn split_link_destination(inside: &str) -> (&str, &str, bool) {
    if let Some(after_open) = inside.strip_prefix('<')
        && let Some(close) = after_open.find('>')
    {
        let end = close + 1;
        let url = &inside[1..end];
        let title = &inside[(end + 1)..];
        return (url, title, true);
    }

    // 標準形式では、タイトル（あれば）は最初の
    // 「エスケープされていない空白」以降に始まる
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

/// `](` の暗黙の開き `(` に対応する閉じ `)` を探す。
fn find_link_close_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    let mut backslash_run = 0usize;
    let mut title_quote: Option<char> = None;
    let mut saw_dest_non_ws = false;
    let mut saw_sep_ws = false;
    let mut in_angle_destination = false;

    for (i, c) in s.char_indices() {
        let escaped = c != '\\' && backslash_run % 2 == 1;

        if c == '\\' {
            backslash_run += 1;
            continue;
        }

        if in_angle_destination {
            if c == '>' && !escaped {
                in_angle_destination = false;
            }
            backslash_run = 0;
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
            if !saw_dest_non_ws && c == '<' {
                in_angle_destination = true;
                saw_dest_non_ws = true;
                backslash_run = 0;
                continue;
            }

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

    // compact_markdown のテスト

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
    fn compact_table_preserves_escaped_pipe_in_cell() {
        assert_eq!(compact_markdown(r"| a\|b      | c |"), r"| a\|b | c |");
    }

    #[test]
    fn compact_table_splits_on_even_backslashes_before_pipe() {
        assert_eq!(compact_markdown(r"| a\\| b | c |"), r"| a\\ | b | c |");
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

    // resolve_markdown_urls のテスト

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

    // find_link_close_paren の直接テスト

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

    // compact_table_row の境界ケース

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
        // lines() は末尾の空行を落とすため "\n\n\n"（4行目が空） は "\n\n" になる
        assert_eq!(compact_markdown("\n\n\n"), "\n\n");
    }

    // resolve_markdown_urls の追加境界ケース

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

    // escape_js_string の追加境界ケース

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

    // fence_marker の直接テスト

    #[test]
    fn fence_marker_backtick_three() {
        assert_eq!(fence_marker("```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_backtick_five() {
        assert_eq!(fence_marker("`````"), Some(('`', 5)));
    }

    #[test]
    fn fence_marker_tilde_three() {
        assert_eq!(fence_marker("~~~"), Some(('~', 3)));
    }

    #[test]
    fn fence_marker_backtick_two_not_enough() {
        assert_eq!(fence_marker("``"), None);
    }

    #[test]
    fn fence_marker_backtick_with_info_string() {
        assert_eq!(fence_marker("```rust"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_non_fence_char() {
        assert_eq!(fence_marker("---"), None);
    }

    #[test]
    fn fence_marker_empty_string() {
        assert_eq!(fence_marker(""), None);
    }

    // compact_markdown の追加境界ケース

    #[test]
    fn compact_unclosed_fence_block() {
        let input = "\
```
| padded           | table           |
no closing fence";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_fence_longer_close() {
        let input = "\
```
| padded           | table           |
`````";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_table_between_fenced_blocks() {
        let input = "\
```
code
```
| padded         | table         |
| -------------- | -------------- |
```
more code
```";
        let expected = "\
```
code
```
| padded | table |
| - | - |
```
more code
```";
        assert_eq!(compact_markdown(input), expected);
    }

    // find_link_close_paren の追加テスト

    #[test]
    fn find_close_paren_title_single_quote() {
        assert_eq!(
            find_link_close_paren("./page 'title with ) paren')"),
            Some(27),
        );
    }

    #[test]
    fn find_close_paren_escaped_backslash_before_paren() {
        // \\) は「バックスラッシュ文字 + エスケープされていない )」を意味する
        assert_eq!(find_link_close_paren("url\\\\)"), Some(5));
    }

    #[test]
    fn find_close_paren_ignores_paren_in_angle_destination() {
        assert_eq!(find_link_close_paren("<./file).md>)"), Some(12));
    }

    // split_link_destination の追加テスト

    #[test]
    fn split_link_destination_empty_angle_brackets() {
        assert_eq!(split_link_destination("<>"), ("", "", true));
    }

    #[test]
    fn split_link_destination_no_closing_angle_bracket() {
        // 標準形式のパースへフォールバックする
        assert_eq!(
            split_link_destination("<no-close"),
            ("<no-close", "", false)
        );
    }

    #[test]
    fn split_link_destination_no_title() {
        assert_eq!(split_link_destination("./page"), ("./page", "", false));
    }

    // resolve_markdown_urls の追加境界ケース

    #[test]
    fn resolve_tel_link_unchanged() {
        let input = "[call](tel:+1234567890)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_javascript_link_unchanged() {
        let input = "[click](javascript:void(0))";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_in_middle_of_text() {
        assert_eq!(
            resolve_markdown_urls("prefix [link](./page) suffix", BASE),
            "prefix [link](https://example.com/docs/en/page) suffix",
        );
    }

    #[test]
    fn resolve_image_with_title() {
        assert_eq!(
            resolve_markdown_urls(r#"![alt](./img.png "photo")"#, BASE),
            r#"![alt](https://example.com/docs/en/img.png "photo")"#,
        );
    }

    #[test]
    fn resolve_angle_bracket_url_with_paren() {
        assert_eq!(
            resolve_markdown_urls("[doc](<./file).md>)", BASE),
            "[doc](<https://example.com/docs/en/file).md>)",
        );
    }
}
