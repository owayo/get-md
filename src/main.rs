mod progress;

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use headless_chrome::protocol::cdp::{Network, Page};
use headless_chrome::{Browser, LaunchOptions, Tab};
use regex::Regex;
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

    /// HTTPS 証明書エラーを無視する（危険: 信頼できる用途に限定）
    #[arg(long)]
    ignore_certificate_errors: bool,

    /// 進捗表示を抑制する
    #[arg(short, long)]
    quiet: bool,

    /// ファイル比較時に日時の差分を無視する。
    /// 日時だけが変わった場合は上書きせず unchanged 扱いにする。
    #[arg(long)]
    ignore_date: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut progress = Progress::new(!cli.quiet);

    let selectors = if cli.selector.is_empty() {
        vec!["body".to_string()]
    } else {
        cli.selector.clone()
    };

    // ブラウザを起動する
    progress.spinner("Launching Chrome...");
    let launch_options = build_launch_options(&cli);

    let browser = Browser::new(launch_options)
        .context("Failed to launch Chrome. Make sure Chrome is installed on your system")?;

    let tab = browser.new_tab().context("Failed to open new tab")?;
    let main_frame_id = tab
        .call_method(Page::GetFrameTree(None))
        .context("Failed to get main frame")?
        .frame_tree
        .frame
        .id;
    let main_response_status = Arc::new(Mutex::new(None::<u32>));
    let main_frame_id_for_handler = main_frame_id.clone();
    let status_for_handler = Arc::clone(&main_response_status);
    tab.register_response_handling(
        "main-document-status",
        Box::new(move |response, _fetch_body| {
            if response.Type == Network::ResourceType::Document
                && response.frame_id.as_ref() == Some(&main_frame_id_for_handler)
            {
                *status_for_handler
                    .lock()
                    .expect("HTTP ステータス記録用 Mutex が poisoned になった") =
                    Some(response.response.status);
            }
        }),
    )
    .context("Failed to register HTTP status handler")?;
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

    // HTTP ステータスコードを確認する（400 以上はエラー）。
    // ページ内 JS は改変可能なため、CDP の Network event から得た値だけを信頼する。
    let status_code = main_response_status
        .lock()
        .expect("HTTP ステータス記録用 Mutex が poisoned になった")
        .unwrap_or(0);

    if status_code >= 400 {
        bail!("HTTP {} — page not saved: {}", status_code, cli.url);
    }

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

    let base_url = document_base_url(&tab).unwrap_or_else(|| cli.url.clone());
    let markdown = compact_markdown(&md_parts.join("\n\n---\n\n"));
    let markdown = resolve_markdown_urls(&markdown, &base_url);
    progress.finish("Converted to Markdown");

    // 出力内容を確定する（末尾改行を保証）
    let output_bytes = if cli.output.is_some() && !markdown.ends_with('\n') {
        format!("{markdown}\n")
    } else {
        markdown
    };

    // 出力
    let old_content = cli.output.as_ref().and_then(|p| std::fs::read(p).ok());
    // File::create 前にファイルの存在を記録する（作成後は常に exists() が true になるため）
    let file_existed_before =
        old_content.is_some() || cli.output.as_ref().is_some_and(|p| p.exists());
    // 削除済み tracked ファイルは書き戻し後に diff が消えるため、書き込み前の状態も保持する。
    let had_unstaged_changes_before = cli.output.as_ref().is_some_and(|p| has_unstaged_changes(p));

    // --ignore-date: 日時だけの差分なら書き込みをスキップ
    let date_only_change = cli.ignore_date
        && cli.output.is_some()
        && old_content
            .as_ref()
            .is_some_and(|old| is_date_only_change(old, output_bytes.as_bytes()));

    if date_only_change {
        let path = cli.output.as_ref().unwrap();
        // 未ステージ変更があれば updated 扱い（file_status と同じ契約）
        let (icon, status) = if had_unstaged_changes_before || has_unstaged_changes(path) {
            ("📝", "updated")
        } else {
            ("✔", "unchanged")
        };
        progress.complete(
            icon,
            &format!("{} → {} ({})", cli.url, path.display(), status),
        );
    } else {
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
                let (icon, status) = file_status(
                    path,
                    file_existed_before,
                    &old_content,
                    output_bytes.as_bytes(),
                    had_unstaged_changes_before,
                );
                progress.complete(
                    icon,
                    &format!("{} → {} ({})", cli.url, path.display(), status),
                );
            }
            None => progress.complete("✔", &cli.url),
        }
    }

    Ok(())
}

fn build_launch_options(cli: &Cli) -> LaunchOptions<'static> {
    LaunchOptions {
        headless: !cli.no_headless,
        path: cli.chrome_path.clone(),
        idle_browser_timeout: idle_browser_timeout(cli.timeout),
        ignore_certificate_errors: cli.ignore_certificate_errors,
        ..LaunchOptions::default()
    }
}

fn document_base_url(tab: &Tab) -> Option<String> {
    tab.evaluate("document.baseURI", false)
        .ok()
        .and_then(|result| result.value)
        .and_then(|value| value.as_str().map(str::to_owned))
        .filter(|base| Url::parse(base).is_ok())
}

/// ファイル出力のステータスを判定する。
///
/// `file_existed_before` は File::create 前に記録したファイルの存在状態。
/// git 管理下のファイルで未ステージの変更があれば常に updated 扱い。
/// それ以外は書き込み前後の内容比較で判定する。
fn file_status<'a>(
    path: &Path,
    file_existed_before: bool,
    old_content: &Option<Vec<u8>>,
    new: &[u8],
    had_unstaged_changes_before: bool,
) -> (&'a str, &'a str) {
    if had_unstaged_changes_before || has_unstaged_changes(path) {
        return ("📝", "updated");
    }

    match old_content {
        // 既存ファイルの読み取りに失敗した場合は新規作成ではないため updated 扱いにする。
        None if file_existed_before => ("📝", "updated"),
        None => ("✨", "created"),
        Some(old) if old != new => ("📝", "updated"),
        Some(_) => ("✔", "unchanged"),
    }
}

/// git diff でファイルに未ステージの変更があるかを調べる
fn has_unstaged_changes(path: &Path) -> bool {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(dir) => dir.join(path),
            Err(_) => return false,
        }
    };

    // 対象パスに最も近い既存ディレクトリを起点に git を実行し、
    // 削除済みの tracked ファイルや repo 外 cwd でも正しく判定する。
    let git_dir = absolute_path
        .ancestors()
        .find(|ancestor| ancestor.is_dir())
        .unwrap_or_else(|| Path::new("."));

    Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["diff", "--name-only", "--"])
        .arg(&absolute_path)
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// 日時パターンにマッチする正規表現（コンパイルは一度だけ）
static DATE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\d{4}[-/]\d{2}[-/]\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2})?(?:[.,]\d+)?(?:Z|[+-]\d{2}:\d{2}|[+-]\d{4})?)?",
    )
    .unwrap()
});

/// 日時パターンを除去した文字列同士を比較し、日時だけの差分かどうかを判定する。
///
/// 両方が有効な UTF-8 であり、かつ双方に日時パターンを含む場合のみ比較する。
/// 非 UTF-8 や日時パターンを含まない側がある場合は false を返す。
fn is_date_only_change(old: &[u8], new: &[u8]) -> bool {
    if old == new {
        return false; // そもそも同一なら日時だけの差分ではない
    }
    let (Ok(old_str), Ok(new_str)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return false; // 非 UTF-8 は安全のため false
    };
    // 双方に日時パターンが含まれていなければ日時だけの差分ではない
    DATE_RE.is_match(old_str)
        && DATE_RE.is_match(new_str)
        && strip_dates(old_str) == strip_dates(new_str)
}

/// 日時パターンを空文字に置換する。
///
/// 対応パターン:
/// - `YYYY-MM-DD HH:MM(:SS)?` / `YYYY/MM/DD HH:MM(:SS)?`
/// - `YYYY-MM-DDTHH:MM(:SS)?(.sss)?(Z|+09:00)?` などの ISO 8601
/// - `YYYY-MM-DD` / `YYYY/MM/DD` (日付のみ)
fn strip_dates(s: &str) -> String {
    DATE_RE.replace_all(s, "").into_owned()
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
    let mut table_state = TableState::Outside;

    md.lines()
        .map(|line| {
            let trimmed_start = line.trim_start();
            if in_fenced_code_block {
                // フェンス内では info string 付きのマーカーを閉じ扱いしない。
                // 閉じフェンスはマーカー以降が空白/タブのみでなければならない。
                if is_closing_fence_line(trimmed_start, fence_char, fence_len) {
                    in_fenced_code_block = false;
                    fence_char = '\0';
                    fence_len = 0;
                    table_state = TableState::Outside;
                    return line.to_string();
                }
                return line.to_string();
            }
            if let Some((marker, marker_len)) = fence_marker(trimmed_start) {
                table_state = TableState::Outside;
                in_fenced_code_block = true;
                fence_char = marker;
                fence_len = marker_len;
                return line.to_string();
            }

            let trimmed = line.trim();
            if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 1 {
                let is_separator = is_table_separator_row(trimmed);
                let normalize_separator = table_state != TableState::Body && is_separator;
                table_state = if normalize_separator || table_state == TableState::Body {
                    TableState::Body
                } else {
                    TableState::HeaderSeen
                };
                if normalize_separator {
                    compact_table_row(trimmed)
                } else {
                    compact_table_row_with_separator(trimmed, false)
                }
            } else {
                table_state = TableState::Outside;
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TableState {
    Outside,
    HeaderSeen,
    Body,
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let marker = line.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }

    let len = line.chars().take_while(|c| *c == marker).count();
    if len >= 3 { Some((marker, len)) } else { None }
}

/// 閉じフェンスとして妥当な行か判定する。
///
/// CommonMark 仕様では、閉じフェンスは「同じ種類のマーカーで開始時と同じ長さ以上」かつ
/// 「マーカー以降は空白/タブのみ」でなければならない。info string 付きの行
/// (例: ` ```rust `) を閉じフェンスとして誤認するとフェンス内コンテンツに対して
/// テーブル圧縮や URL 解決が誤って実行されるため、閉じ判定時はこの関数を使う。
fn is_closing_fence_line(line: &str, marker: char, min_len: usize) -> bool {
    // 行末の CR は line ending の一部。`find_next_link_candidate` 側は `\n` で
    // 分割するため `\r` が残り得るので、判定前に落として CRLF 入力でも閉じ判定が
    // 成立するようにする。
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some((found_marker, found_len)) = fence_marker(line) else {
        return false;
    };
    if found_marker != marker || found_len < min_len {
        return false;
    }
    line.chars().skip(found_len).all(|c| c == ' ' || c == '\t')
}

/// ブロッククォート記号を取り除いた位置にあるフェンスマーカーを検出する。
fn fence_marker_after_blockquote(line: &str) -> Option<(char, usize)> {
    strip_blockquote_markers(line).and_then(fence_marker)
}

/// 行頭のインデントとブロッククォート記号 (`>`) を取り除いた残りを返す。
/// `fence_marker_after_blockquote` と `is_closing_fence_after_blockquote` で共通化。
fn strip_blockquote_markers(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    while let Some(after_marker) = rest.strip_prefix('>') {
        rest = after_marker
            .strip_prefix(' ')
            .unwrap_or(after_marker)
            .trim_start();
    }
    Some(rest)
}

/// `fence_marker_after_blockquote` で得たマーカーが閉じフェンスとして妥当か判定する。
fn is_closing_fence_after_blockquote(line: &str, marker: char, min_len: usize) -> bool {
    let Some(rest) = strip_blockquote_markers(line) else {
        return false;
    };
    is_closing_fence_line(rest, marker, min_len)
}

fn compact_table_row(row: &str) -> String {
    compact_table_row_with_separator(row, is_table_separator_row(row))
}

fn compact_table_row_with_separator(row: &str, normalize_separator: bool) -> String {
    let inner = &row[1..row.len() - 1];
    let cells: Vec<String> = split_unescaped_table_cells(inner)
        .into_iter()
        .map(|cell| {
            let t = cell.trim();
            if normalize_separator && is_table_separator_cell(t) {
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

fn is_table_separator_row(row: &str) -> bool {
    let inner = &row[1..row.len() - 1];
    let cells = split_unescaped_table_cells(inner);
    !cells.is_empty()
        && cells
            .iter()
            .all(|cell| is_table_separator_cell(cell.trim()))
}

fn is_table_separator_cell(cell: &str) -> bool {
    !cell.is_empty() && cell.chars().all(|c| c == '-' || c == ':')
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
    // ループをまたいで、cursor 以前のコード領域外で未閉鎖の `[` を引き継ぐ。
    // これにより `[![inner](img)](outer)` のように外側 `[` が cursor より前に
    // ある場合でも、後続走査で外側リンクを認識できる。
    let mut pending_open_brackets: usize = 0;

    while let (Some(open), open_count_at_link) =
        find_next_link_candidate(md, cursor, pending_open_brackets)
    {
        let inside_start = open + 2;

        result.push_str(&md[cursor..inside_start]);

        let part = &md[inside_start..];
        if let Some(close) = find_link_close_paren(part) {
            let inside = &part[..close];
            let (url, title, use_angle_brackets) = split_link_destination(inside);
            let resolved_input = unescape_markdown_destination(url);

            if !url.is_empty() {
                match base.join(&resolved_input) {
                    Ok(resolved) => {
                        write_resolved_url(&mut result, resolved.as_str(), use_angle_brackets);
                    }
                    Err(_) => {
                        write_resolved_url(&mut result, url, use_angle_brackets);
                    }
                }
            } else if use_angle_brackets {
                result.push_str("<>");
            }
            result.push_str(title);
            result.push(')');
            cursor = inside_start + close + 1;
        } else {
            // 閉じ `)` が見つからなければこの `](` はリンクとして扱えない。
            // ただし後続の正常なリンクまで諦めずに、カーソルだけ `](` の直後へ進めて走査を継続する。
            // 既に `cursor..inside_start` は結果に書き込み済みなので二重出力にはならない。
            cursor = inside_start;
        }
        // リンクとして認識された `]` 1 つ分のペアを除外し、残った未閉鎖 `[` を引き継ぐ。
        pending_open_brackets = open_count_at_link.saturating_sub(1);
    }

    result.push_str(&md[cursor..]);
    result
}

/// 解決済みの URL を Markdown リンク先として書き出す。
///
/// 標準形式の場合に `(` と `)` のバランスが崩れていると Markdown リンクが壊れるため、
/// アンバランスなパーレンを含む URL は山括弧形式 `<...>` に切り替えて出力する。
/// 山括弧形式が指定されている場合は常に `<...>` で出力する。
fn write_resolved_url(out: &mut String, url: &str, use_angle_brackets: bool) {
    let needs_angle = use_angle_brackets || !url_has_balanced_parens(url);
    if needs_angle {
        out.push('<');
        out.push_str(url);
        out.push('>');
    } else {
        out.push_str(url);
    }
}

/// URL 文字列の `(` と `)` がバランスしているかを判定する。
///
/// CommonMark の標準形式リンク先ではアンバランスなパーレンは許容されないため、
/// アンバランスな場合は山括弧形式に切り替える必要がある。
fn url_has_balanced_parens(url: &str) -> bool {
    let mut depth: i32 = 0;
    for c in url.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Markdown リンク先に含まれる最小限のバックスラッシュエスケープを
/// URL 解決前の実 URL 文字列へ戻す。
///
/// `\ ` `\(` `\)` `\<` `\>` をそのまま `Url::join` に渡すと、
/// バックスラッシュがパス区切りや文字列の一部として解釈されてしまう。
fn unescape_markdown_destination(url: &str) -> String {
    let mut result = String::with_capacity(url.len());
    let mut chars = url.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(' ' | '(' | ')' | '<' | '>') = chars.peek().copied()
        {
            result.push(chars.next().expect("peek 済みの文字が存在する"));
            continue;
        }

        result.push(ch);
    }

    result
}

/// フェンスコードブロックとインラインコードを除外しつつ、次の `](` を探す。
///
/// コード領域外で開いている `[` を前方走査でカウントし、エスケープされた
/// `\]` `\`` `\[` を正しくリテラルとして扱う。`initial_open_brackets` は
/// `start` 位置より前から引き継いだ未閉鎖 `[` の数（外側リンク対応のため）。
/// 戻り値は `(](`の位置, 検出時点の未閉鎖 `[` 数)`。
fn find_next_link_candidate(
    md: &str,
    start: usize,
    initial_open_brackets: usize,
) -> (Option<usize>, usize) {
    let mut cursor = start;
    let mut line_start = start == 0 || md[..start].ends_with('\n');
    let mut in_fenced_code_block = false;
    let mut fence_char = '\0';
    let mut fence_len = 0usize;
    let mut inline_code_len = 0usize;
    // コード領域外で未閉鎖の `[` の数を追跡する。`]` で減算する。
    let mut open_bracket_count: usize = initial_open_brackets;
    // 直前の連続したバックスラッシュの数。エスケープ判定を O(1) で行うために保持する。
    // `start` 位置以前のバックスラッシュ列も引き継ぐことで、途中再開時も
    // 旧 `is_escaped_markdown_char(md, start)` と等価な判定になる。
    // UTF-8 の継続バイトは 0x80-0xBF なので 0x5C(`\`) と衝突せず、
    // バイト列の末尾を逆走査するだけで安全に数えられる。
    let mut backslash_run: usize = md.as_bytes()[..start]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count();

    while cursor < md.len() {
        if line_start && inline_code_len == 0 {
            let line_end = md[cursor..]
                .find('\n')
                .map(|offset| cursor + offset)
                .unwrap_or(md.len());
            let line = &md[cursor..line_end];
            let mut handled_as_fence = false;
            if in_fenced_code_block {
                // 閉じフェンスは marker 以降が空白/タブのみのときだけ閉じる。
                if is_closing_fence_after_blockquote(line, fence_char, fence_len) {
                    in_fenced_code_block = false;
                    fence_char = '\0';
                    fence_len = 0;
                }
                handled_as_fence = true;
            } else if let Some((marker, marker_len)) = fence_marker_after_blockquote(line) {
                in_fenced_code_block = true;
                fence_char = marker;
                fence_len = marker_len;
                handled_as_fence = true;
            }
            if handled_as_fence {
                cursor = line_end;
                if cursor < md.len() {
                    cursor += 1;
                }
                line_start = true;
                backslash_run = 0;
                continue;
            }
            line_start = false;
        }

        let rest = &md[cursor..];

        // フェンスコード内は内容を解釈せずに 1 文字ずつ進める
        if in_fenced_code_block {
            let Some(ch) = rest.chars().next() else {
                break;
            };
            cursor += ch.len_utf8();
            if ch == '\n' {
                line_start = true;
            }
            backslash_run = 0;
            continue;
        }

        // インラインコード内は同じ長さのバッククォート列で閉じる
        if inline_code_len > 0 {
            if rest.starts_with('`') {
                let tick_len = rest.chars().take_while(|c| *c == '`').count();
                if tick_len == inline_code_len {
                    inline_code_len = 0;
                }
                cursor += tick_len;
                backslash_run = 0;
                continue;
            }
            let Some(ch) = rest.chars().next() else {
                break;
            };
            cursor += ch.len_utf8();
            if ch == '\n' {
                line_start = true;
            }
            backslash_run = 0;
            continue;
        }

        let escaped = backslash_run % 2 == 1;

        // `](` の検出: `]` がエスケープされておらず、対応する `[` が開いている場合のみ
        if rest.starts_with("](") && !escaped && open_bracket_count > 0 {
            return (Some(cursor), open_bracket_count);
        }

        // バッククォート: エスケープされていない場合のみインラインコード開始候補
        if rest.starts_with('`') && !escaped {
            let tick_len = rest.chars().take_while(|c| *c == '`').count();
            if has_matching_inline_code_closer(md, cursor + tick_len, tick_len) {
                inline_code_len = tick_len;
            }
            cursor += tick_len;
            backslash_run = 0;
            continue;
        }

        let Some(ch) = rest.chars().next() else {
            break;
        };
        // コード領域外で、エスケープされていない `[` `]` をカウントする
        if !escaped {
            if ch == '[' {
                open_bracket_count = open_bracket_count.saturating_add(1);
            } else if ch == ']' && open_bracket_count > 0 {
                open_bracket_count -= 1;
            }
        }
        if ch == '\\' {
            backslash_run += 1;
        } else {
            backslash_run = 0;
        }
        cursor += ch.len_utf8();
        if ch == '\n' {
            line_start = true;
        }
    }

    (None, open_bracket_count)
}

/// 開始位置より後ろに、同じ長さのバッククォート列が存在するかを調べる。
///
/// CommonMark では未閉鎖のバッククォート列はリテラルとして扱われるため、
/// 対応する閉じ列が見つかる場合だけインラインコードとして扱う。
fn has_matching_inline_code_closer(md: &str, start: usize, tick_len: usize) -> bool {
    let mut cursor = start;

    while cursor < md.len() {
        let rest = &md[cursor..];
        if rest.starts_with('`') {
            let run_len = rest.chars().take_while(|c| *c == '`').count();
            if run_len == tick_len {
                return true;
            }
            cursor += run_len;
            continue;
        }

        let ch = rest.chars().next().expect("cursor は文字境界上にある");
        cursor += ch.len_utf8();
    }

    false
}

/// 指定位置の Markdown 記号が、直前のバックスラッシュでエスケープされているかを調べる。
///
/// 本体の `find_next_link_candidate` は前方走査中に `backslash_run` を
/// 保持して O(1) で判定するため、現在は単体テストのみで参照する。
#[cfg(test)]
fn is_escaped_markdown_char(md: &str, idx: usize) -> bool {
    let bytes = md.as_bytes();
    let mut cursor = idx;
    let mut backslash_count = 0usize;

    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslash_count += 1;
        cursor -= 1;
    }

    backslash_count % 2 == 1
}

/// Markdown のリンク先を URL とタイトルに分割する。
///
/// 対応形式:
/// - 標準形式: `./path "title"`
/// - 山括弧形式: `<./path with space> "title"`
fn split_link_destination(inside: &str) -> (&str, &str, bool) {
    let body = inside.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if body.is_empty() {
        return ("", inside, false);
    }

    // `]( "title")` は空のリンク先 + title なので、title を URL と誤認しない。
    if body.len() < inside.len() && matches!(body.chars().next(), Some('"' | '\'')) {
        return ("", inside, false);
    }

    if let Some(after_open) = body.strip_prefix('<') {
        // エスケープされていない `>` を探す（`\>` はスキップ）
        let mut backslash_run = 0usize;
        for (off, ch) in after_open.char_indices() {
            if ch == '\\' {
                backslash_run += 1;
                continue;
            }
            let escaped = backslash_run % 2 == 1;
            if ch == '>' && !escaped {
                let end = 1 + off; // body 上の '>' の位置
                let url = &body[1..end];
                let title = &body[(end + 1)..];
                return (url, title, true);
            }
            backslash_run = 0;
        }
    }

    // 標準形式では、タイトル（あれば）は最初の
    // 「エスケープされていない空白」以降に始まる
    let mut backslash_run = 0usize;
    for (i, c) in body.char_indices() {
        if c == '\\' {
            backslash_run += 1;
            continue;
        }
        let escaped = backslash_run % 2 == 1;
        if c.is_ascii_whitespace() && !escaped {
            return (&body[..i], &body[i..], false);
        }
        backslash_run = 0;
    }
    (body, "", false)
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
            // バックスラッシュ自体もリンク先の非空白文字。これを反映しないと、
            // `\\<a)` のように先頭が `\` で始まるリンク先で後続の `<` が
            // 山括弧形式の開始として誤認される。
            if depth == 1 && !in_angle_destination && title_quote.is_none() {
                saw_dest_non_ws = true;
                saw_sep_ws = false;
            }
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
            if !saw_dest_non_ws && c == '<' && !escaped {
                in_angle_destination = true;
                saw_dest_non_ws = true;
                saw_sep_ws = false;
                backslash_run = 0;
                continue;
            }

            if c.is_ascii_whitespace() && !escaped {
                // 先頭空白 + quote は「空のリンク先 + title」として扱う。
                saw_sep_ws = true;
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

    /// テスト用の `find_next_link_candidate` ラッパー。
    /// テストでは引き継ぎ状態を 0 で固定し、位置のみを返す。
    fn find_next_link_candidate(md: &str, start: usize) -> Option<usize> {
        super::find_next_link_candidate(md, start, 0).0
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("failed to get current time")
                .as_nanos()
        ))
    }

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("failed to execute git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr),
        );
    }

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
        assert!(!cli.ignore_certificate_errors);
        assert!(!cli.quiet);
        assert!(!build_launch_options(&cli).ignore_certificate_errors);
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
            "--ignore-certificate-errors",
            "-q",
        ])
        .unwrap();
        assert_eq!(cli.url, "https://example.com");
        assert_eq!(cli.selector, vec!["article", ".content"]);
        assert_eq!(cli.output.as_ref().unwrap().to_str().unwrap(), "out.md");
        assert_eq!(cli.wait, 5);
        assert_eq!(cli.timeout, 60);
        assert!(cli.no_headless);
        assert!(cli.no_cache);
        assert!(cli.ignore_certificate_errors);
        assert!(cli.quiet);
        assert!(build_launch_options(&cli).ignore_certificate_errors);
    }

    #[test]
    fn launch_options_follow_cli_browser_settings() {
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "--chrome-path",
            "/usr/bin/chromium",
            "--no-headless",
            "--ignore-certificate-errors",
            "--timeout",
            "45",
        ])
        .unwrap();

        let options = build_launch_options(&cli);
        assert!(!options.headless);
        assert_eq!(options.path, Some(PathBuf::from("/usr/bin/chromium")));
        assert_eq!(options.idle_browser_timeout, Duration::from_secs(75));
        assert!(options.ignore_certificate_errors);
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
    fn compact_table_preserves_separator_like_data_cells() {
        let input = "| key | value |\n| --- | --- |\n| dash | -- |\n| colon | : |";
        let expected = "| key | value |\n| - | - |\n| dash | -- |\n| colon | : |";
        assert_eq!(compact_markdown(input), expected);
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
    fn resolve_link_with_single_quoted_title() {
        assert_eq!(
            resolve_markdown_urls("[link](./page 'Title')", BASE),
            "[link](https://example.com/docs/en/page 'Title')",
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
    fn resolve_title_only_link_keeps_empty_destination() {
        assert_eq!(
            resolve_markdown_urls(r#"[link]( "Title")"#, BASE),
            r#"[link]( "Title")"#,
        );
    }

    #[test]
    fn resolve_single_quoted_title_only_link_keeps_empty_destination() {
        assert_eq!(
            resolve_markdown_urls("[link]( 'Title')", BASE),
            "[link]( 'Title')",
        );
    }

    #[test]
    fn resolve_title_only_link_ignores_paren_in_title() {
        assert_eq!(
            resolve_markdown_urls(r#"[link]( "Title ) text") [next](./page)"#, BASE),
            r#"[link]( "Title ) text") [next](https://example.com/docs/en/page)"#,
        );
    }

    #[test]
    fn resolve_standard_url_with_escaped_space() {
        assert_eq!(
            resolve_markdown_urls(r"[doc](./my\ file.md)", BASE),
            "[doc](https://example.com/docs/en/my%20file.md)",
        );
    }

    #[test]
    fn resolve_standard_url_with_escaped_parentheses() {
        assert_eq!(
            resolve_markdown_urls(r"[doc](./file\(draft\).md)", BASE),
            "[doc](https://example.com/docs/en/file(draft).md)",
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

    #[test]
    fn resolve_relative_link_with_unbalanced_open_paren_in_base_uses_angle_brackets() {
        assert_eq!(
            resolve_markdown_urls(
                "[link](./next.md)",
                "https://example.com/docs/(draft/page.md"
            ),
            "[link](<https://example.com/docs/(draft/next.md>)",
        );
    }

    #[test]
    fn resolve_relative_link_with_unbalanced_close_paren_in_base_uses_angle_brackets() {
        assert_eq!(
            resolve_markdown_urls(
                "[link](./next.md)",
                "https://example.com/docs/draft)/page.md"
            ),
            "[link](<https://example.com/docs/draft)/next.md>)",
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
    fn resolve_link_with_leading_destination_whitespace() {
        assert_eq!(
            resolve_markdown_urls("[doc](  ./page.md \"Title\")", BASE),
            "[doc](https://example.com/docs/en/page.md \"Title\")",
        );
    }

    #[test]
    fn resolve_angle_bracket_link_with_leading_destination_whitespace() {
        assert_eq!(
            resolve_markdown_urls("[doc](  <./my file.md> \"Title\")", BASE),
            "[doc](<https://example.com/docs/en/my%20file.md> \"Title\")",
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
    fn find_close_paren_ignores_paren_in_title_only_link() {
        let input = r#" "title ) marker")"#;
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    #[test]
    fn split_link_destination_standard_with_title() {
        assert_eq!(
            split_link_destination(r#"./page "Title""#),
            ("./page", r#" "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_standard_with_single_quoted_title() {
        assert_eq!(
            split_link_destination("./page 'Title'"),
            ("./page", " 'Title'", false),
        );
    }

    #[test]
    fn split_link_destination_standard_with_leading_whitespace() {
        assert_eq!(
            split_link_destination(r#"  ./page.md "Title""#),
            ("./page.md", r#" "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_title_only_with_leading_whitespace() {
        assert_eq!(
            split_link_destination(r#"  "Title""#),
            ("", r#"  "Title""#, false),
        );
    }

    #[test]
    fn split_link_destination_angle_bracket_with_leading_whitespace() {
        assert_eq!(
            split_link_destination(r#"  <./my file.md> "Title""#),
            ("./my file.md", r#" "Title""#, true),
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
    fn resolve_inline_code_link_unchanged() {
        let input = "`[code](./skip)` and [link](./page)";
        let expected = "`[code](./skip)` and [link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_unclosed_inline_code_backticks_are_literal() {
        let input = "`literal [link](./page)";
        let expected = "`literal [link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_non_link_bracket_paren_sequence_unchanged() {
        let input = "text ](./not-a-link) and [link](./page)";
        let expected = "text ](./not-a-link) and [link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_nested_brackets_in_link_text() {
        let input = "[outer [inner]](./page)";
        let expected = "[outer [inner]](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
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

    // file_status のテスト

    #[test]
    fn file_status_new_file() {
        let (icon, status) = file_status(Path::new("dummy"), false, &None, b"content", false);
        assert_eq!(icon, "✨");
        assert_eq!(status, "created");
    }

    #[test]
    fn file_status_content_changed() {
        let old = Some(b"old content".to_vec());
        let (icon, status) = file_status(Path::new("dummy"), true, &old, b"new content", false);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");
    }

    #[test]
    fn file_status_content_unchanged_no_git() {
        // 存在しないパスなので git diff は空を返す → unchanged
        let content = b"same content";
        let old = Some(content.to_vec());
        let (icon, status) =
            file_status(Path::new("/nonexistent/path"), true, &old, content, false);
        assert_eq!(icon, "✔");
        assert_eq!(status, "unchanged");
    }

    #[test]
    fn file_status_existing_file_without_old_content_is_updated() {
        let dir = make_temp_dir("get-md-status-test");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("existing.md");
        std::fs::write(&path, b"old").expect("failed to write fixture file");

        let (icon, status) = file_status(path.as_path(), true, &None, b"new", false);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_unstaged_changes_detects_tracked_file_from_target_repo() {
        let dir = make_temp_dir("get-md-git-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("tracked.md");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);

        std::fs::write(&path, b"old").expect("failed to write tracked file");
        git(&dir, &["add", "tracked.md"]);
        git(&dir, &["commit", "-m", "init"]);

        std::fs::write(&path, b"new").expect("failed to update tracked file");

        assert!(has_unstaged_changes(&path));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn has_unstaged_changes_returns_false_outside_git_repo() {
        let dir = make_temp_dir("get-md-no-git-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("plain.md");
        std::fs::write(&path, b"content").expect("failed to write fixture file");

        // git 管理外の出力先では diff 判定に失敗しても安全側の false に倒す。
        assert!(!has_unstaged_changes(&path));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_status_deleted_tracked_file_is_updated() {
        let dir = make_temp_dir("get-md-deleted-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("tracked.md");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);

        std::fs::write(&path, b"old").expect("failed to write tracked file");
        git(&dir, &["add", "tracked.md"]);
        git(&dir, &["commit", "-m", "init"]);

        std::fs::remove_file(&path).expect("failed to delete tracked file");

        assert!(has_unstaged_changes(&path));
        let (icon, status) = file_status(&path, false, &None, b"new", true);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_status_restored_deleted_tracked_file_same_content_is_updated() {
        let dir = make_temp_dir("get-md-restored-deleted-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("tracked.md");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);

        std::fs::write(&path, b"same").expect("failed to write tracked file");
        git(&dir, &["add", "tracked.md"]);
        git(&dir, &["commit", "-m", "init"]);

        std::fs::remove_file(&path).expect("failed to delete tracked file");
        let had_unstaged_changes_before = has_unstaged_changes(&path);
        std::fs::write(&path, b"same").expect("failed to restore tracked file");

        let (icon, status) = file_status(&path, false, &None, b"same", had_unstaged_changes_before);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // compact_markdown の追加エッジケース

    #[test]
    fn compact_table_row_minimal_pipe_pair() {
        // "||" は starts_with('|') && ends_with('|') && len > 1 だが
        // 内部が空のため空セルとして処理される
        assert_eq!(compact_markdown("||"), "|  |");
    }

    // resolve_markdown_urls の追加エッジケース

    #[test]
    fn resolve_unclosed_link_paren() {
        // 閉じ括弧がない場合、残りの文字列をそのまま出力する
        let input = "[link](./path";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_consecutive_link_markers_without_url() {
        let input = "text](not-a-link) more";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    // find_link_close_paren の追加エッジケース

    #[test]
    fn find_close_paren_only_backslashes() {
        // バックスラッシュだけの入力は閉じ括弧なし
        assert_eq!(find_link_close_paren("\\\\\\"), None);
    }

    // split_unescaped_table_cells の追加テスト

    #[test]
    fn split_unescaped_table_cells_no_pipe() {
        assert_eq!(split_unescaped_table_cells("abc"), vec!["abc"]);
    }

    #[test]
    fn split_unescaped_table_cells_trailing_backslashes() {
        // 末尾のバックスラッシュはセル分割に影響しない
        assert_eq!(split_unescaped_table_cells("a\\\\|b"), vec!["a\\\\", "b"],);
    }

    // find_link_close_paren の追加エッジケース

    #[test]
    fn find_close_paren_empty_string() {
        // 空文字列には閉じ括弧がない
        assert_eq!(find_link_close_paren(""), None);
    }

    #[test]
    fn find_close_paren_consecutive_open_parens() {
        // (())) → depth=3→2→1→0 で最後の ) で閉じる
        assert_eq!(find_link_close_paren("(()))"), Some(4));
    }

    // split_unescaped_table_cells の追加テスト

    #[test]
    fn split_unescaped_table_cells_escaped_pipe() {
        // エスケープされたパイプはセル区切りとして扱わない
        assert_eq!(split_unescaped_table_cells(r"a\|b|c"), vec![r"a\|b", "c"],);
    }

    #[test]
    fn split_unescaped_table_cells_multiple() {
        assert_eq!(
            split_unescaped_table_cells("a|b|c|d"),
            vec!["a", "b", "c", "d"],
        );
    }

    #[test]
    fn split_unescaped_table_cells_empty_inner() {
        // パイプ間が空のケース
        assert_eq!(split_unescaped_table_cells("||"), vec!["", "", ""],);
    }

    // compact_markdown の追加テスト

    #[test]
    fn compact_table_many_columns() {
        assert_eq!(
            compact_markdown("| a   | b   | c   | d   | e   |"),
            "| a | b | c | d | e |",
        );
    }

    #[test]
    fn compact_table_separator_only_colons() {
        // コロンのみのセパレータセルは配置指定として保持
        assert_eq!(compact_markdown("| :-: |"), "| :-: |");
    }

    // escape_js_string の追加テスト

    #[test]
    fn escape_null_byte() {
        // NULバイトはそのまま通過する（CSS セレクタには通常含まれない）
        assert_eq!(escape_js_string("a\0b"), "\"a\0b\"");
    }

    // resolve_markdown_urls の追加テスト

    #[test]
    fn resolve_multiple_links_on_separate_lines() {
        let input = "[a](./one)\n[b](./two)";
        let expected = "[a](https://example.com/docs/en/one)\n[b](https://example.com/docs/en/two)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_fenced_code_block_link_unchanged() {
        let input = "\
```md
[code](./skip)
```
[link](./page)";
        let expected = "\
```md
[code](./skip)
```
[link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_empty_input() {
        assert_eq!(resolve_markdown_urls("", BASE), "");
    }

    #[test]
    fn resolve_only_link_marker_no_url() {
        // ]( だけで終わる入力
        assert_eq!(resolve_markdown_urls("[text](", BASE), "[text](");
    }

    // compact_markdown: フェンス文字の不一致テスト

    #[test]
    fn compact_mismatched_fence_does_not_close() {
        // バックティックで開いたブロックはチルダでは閉じない
        let input = "\
```
| padded           | table           |
~~~
| also padded      | table           |
```";
        let expected = "\
```
| padded           | table           |
~~~
| also padded      | table           |
```";
        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_table_line_not_ending_with_pipe() {
        // パイプで始まるがパイプで終わらない行はテーブルとして扱わない
        assert_eq!(compact_markdown("| not a table"), "| not a table");
    }

    #[test]
    fn compact_shorter_close_fence_ignored() {
        // 開始フェンスより短い閉じフェンスはブロックを閉じない
        let input = "\
`````
| padded           | table           |
```
| still inside     | fence           |
`````";
        assert_eq!(compact_markdown(input), input);
    }

    // resolve_markdown_urls: 追加エッジケース

    #[test]
    fn resolve_dot_url() {
        // カレントディレクトリ参照
        assert_eq!(
            resolve_markdown_urls("[link](.)", BASE),
            "[link](https://example.com/docs/en/)",
        );
    }

    #[test]
    fn resolve_double_dot_url() {
        // 親ディレクトリ参照
        assert_eq!(
            resolve_markdown_urls("[link](..)", BASE),
            "[link](https://example.com/docs/)",
        );
    }

    // find_link_close_paren: 追加エッジケース

    #[test]
    fn find_close_paren_angle_dest_then_title_with_paren() {
        // 山括弧リンク先の後にタイトル内の括弧がある場合
        assert_eq!(
            find_link_close_paren(r#"<url> "title (with parens)")"#),
            Some(27),
        );
    }

    // split_unescaped_table_cells: 追加エッジケース

    #[test]
    fn split_unescaped_table_cells_odd_backslashes_before_pipe() {
        // 奇数個のバックスラッシュ + パイプ → エスケープ（分割しない）
        assert_eq!(split_unescaped_table_cells(r"a\\\|b"), vec![r"a\\\|b"],);
    }

    // compact_markdown: インデント付きフェンスのテスト

    #[test]
    fn compact_indented_fence_preserves_table() {
        // インデント付きのフェンス行もフェンスとして認識される
        let input = "\
  ```
| padded           | table           |
  ```";
        assert_eq!(compact_markdown(input), input);
    }

    // find_link_close_paren: タイトル引用符の認識条件テスト

    #[test]
    fn find_close_paren_quote_without_space_not_title() {
        // URL直後の引用符（空白なし）はタイトル開始として扱わない
        // → `"` の中の `)` も通常の閉じ括弧として深度を減少させる
        assert_eq!(find_link_close_paren(r#"url"title)"#), Some(9));
    }

    #[test]
    fn find_close_paren_quote_with_space_is_title() {
        // 空白の後の引用符はタイトル開始として扱う
        // → タイトル内の `)` は無視される
        assert_eq!(
            find_link_close_paren(r#"url "title with ) paren")"#),
            Some(24),
        );
    }

    // compact_table_row: 空白のみのセルのテスト

    #[test]
    fn compact_table_whitespace_only_cells() {
        // 空白のみのセルはトリム後に空文字列になる
        assert_eq!(compact_markdown("|   |   |"), "|  |  |");
    }

    // resolve_markdown_urls: 連続する `](` パターンのテスト

    #[test]
    fn resolve_link_with_empty_angle_brackets() {
        // 空の山括弧リンク先
        assert_eq!(resolve_markdown_urls("[link](<>)", BASE), "[link](<>)");
    }

    // split_link_destination: 山括弧内に山括弧がないケース

    #[test]
    fn split_link_destination_angle_bracket_url_only() {
        assert_eq!(split_link_destination("<./page>"), ("./page", "", true),);
    }

    // escape_js_string: 複数のエスケープ対象が連続するケース

    #[test]
    fn escape_consecutive_backslashes_and_quotes() {
        assert_eq!(escape_js_string(r#"\""#), r#""\\\"""#);
    }

    // compact_markdown: フェンスブロック直後のテーブル行

    #[test]
    fn compact_table_immediately_after_fence_close() {
        let input = "\
```
code
```
| a         | b         |";
        let expected = "\
```
code
```
| a | b |";
        assert_eq!(compact_markdown(input), expected);
    }

    // file_status: 同一内容で git 管理外のパスの場合

    #[test]
    fn file_status_same_content_nonexistent_dir() {
        let content = b"hello";
        let old = Some(content.to_vec());
        let (icon, status) = file_status(
            Path::new("/tmp/nonexistent_dir_12345/file.txt"),
            true,
            &old,
            content,
            false,
        );
        assert_eq!(icon, "✔");
        assert_eq!(status, "unchanged");
    }

    // is_date_only_change / strip_dates のテスト

    #[test]
    fn date_only_change_detected() {
        let old = b"*Generated: 2026-03-25 16:01 - auto-generated*\ncontent here";
        let new = b"*Generated: 2026-03-25 17:02 - auto-generated*\ncontent here";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_date_only_format() {
        let old = b"Last updated: 2026-03-25\nHello";
        let new = b"Last updated: 2026-03-26\nHello";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_iso8601() {
        let old = b"timestamp: 2026-03-25T16:01:30\ndata";
        let new = b"timestamp: 2026-03-26T09:00:00\ndata";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_iso8601_z_suffix() {
        let old = b"timestamp: 2026-03-25T16:01:30Z\ndata";
        let new = b"timestamp: 2026-03-26T09:00:00Z\ndata";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_iso8601_fractional_and_offset() {
        let old = b"timestamp: 2026-03-25T16:01:30.123+09:00\ndata";
        let new = b"timestamp: 2026-03-26T09:00:00.456+09:00\ndata";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_slash_date() {
        let old = b"date: 2026/03/25 16:01\ndata";
        let new = b"date: 2026/03/26 09:00\ndata";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn not_date_only_change_content_differs() {
        let old = b"*Generated: 2026-03-25 16:01*\nold content";
        let new = b"*Generated: 2026-03-25 17:02*\nnew content";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn not_date_only_change_identical() {
        let content = b"*Generated: 2026-03-25 16:01*\ncontent";
        assert!(!is_date_only_change(content, content));
    }

    #[test]
    fn strip_dates_removes_datetime() {
        assert_eq!(
            strip_dates("Generated: 2026-03-25 16:01 - auto"),
            "Generated:  - auto"
        );
    }

    #[test]
    fn strip_dates_removes_datetime_with_seconds() {
        assert_eq!(strip_dates("at 2026-03-25 16:01:30 done"), "at  done");
    }

    #[test]
    fn strip_dates_removes_iso8601_with_timezone() {
        assert_eq!(
            strip_dates("at 2026-03-25T16:01:30.123+09:00 done"),
            "at  done"
        );
    }

    #[test]
    fn strip_dates_removes_date_only() {
        assert_eq!(strip_dates("on 2026-03-25 ok"), "on  ok");
    }

    // compact_markdown: 完全なテーブル（ヘッダ + セパレータ + データ行）

    #[test]
    fn compact_full_table_header_separator_data() {
        let input = "\
| Name         | Age    |
| ------------ | ------ |
| Alice        | 30     |";
        let expected = "\
| Name | Age |
| - | - |
| Alice | 30 |";
        assert_eq!(compact_markdown(input), expected);
    }

    // resolve_markdown_urls: パーセントエンコードされた URL

    #[test]
    fn resolve_percent_encoded_url() {
        assert_eq!(
            resolve_markdown_urls("[link](./path%20with%20spaces)", BASE),
            "[link](https://example.com/docs/en/path%20with%20spaces)",
        );
    }

    // fence_marker: チルダ + info string

    #[test]
    fn fence_marker_tilde_with_info_string() {
        assert_eq!(fence_marker("~~~python"), Some(('~', 3)));
    }

    // idle_browser_timeout: ゼロ秒のタイムアウト

    #[test]
    fn idle_browser_timeout_zero() {
        assert_eq!(idle_browser_timeout(0), Duration::from_secs(30));
    }

    // resolve_markdown_urls: リファレンスリンクスタイルは変換しない

    #[test]
    fn resolve_reference_style_link_unchanged() {
        // [text][ref] はリンク先を含まないため変換対象外
        let input = "[text][ref]\n\n[ref]: ./page";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    // escape_js_string: 長い入力文字列

    #[test]
    fn escape_long_selector() {
        let selector = "div.container > ul.list > li:nth-child(3) > a.link";
        let escaped = escape_js_string(selector);
        assert!(escaped.starts_with('"'));
        assert!(escaped.ends_with('"'));
        // 特殊文字がないのでそのまま
        assert_eq!(escaped, format!("\"{selector}\""));
    }

    // is_escaped_markdown_char のテスト

    #[test]
    fn escaped_char_single_backslash() {
        // `\[` の `[` はエスケープされている
        assert!(is_escaped_markdown_char(r"\[", 1));
    }

    #[test]
    fn escaped_char_no_backslash() {
        // `[` の前にバックスラッシュがない
        assert!(!is_escaped_markdown_char("[", 0));
    }

    #[test]
    fn escaped_char_double_backslash() {
        // `\\[` の `[` はエスケープされていない（バックスラッシュ同士が打ち消し合う）
        assert!(!is_escaped_markdown_char(r"\\[", 2));
    }

    #[test]
    fn escaped_char_triple_backslash() {
        // `\\\[` の `[` はエスケープされている（3つ目のバックスラッシュが有効）
        assert!(is_escaped_markdown_char(r"\\\[", 3));
    }

    #[test]
    fn escaped_char_at_start() {
        // 先頭文字は前にバックスラッシュがないのでエスケープされていない
        assert!(!is_escaped_markdown_char("a", 0));
    }

    // find_next_link_candidate のテスト

    #[test]
    fn link_candidate_simple() {
        let md = "[link](url)";
        assert_eq!(find_next_link_candidate(md, 0), Some(5));
    }

    #[test]
    fn link_candidate_no_link() {
        assert_eq!(find_next_link_candidate("plain text", 0), None);
    }

    #[test]
    fn link_candidate_skips_inline_code() {
        // インラインコード内の `](` は無視される
        let md = "`[code](skip)` [link](url)";
        assert_eq!(find_next_link_candidate(md, 0), Some(20));
    }

    #[test]
    fn link_candidate_skips_fenced_code() {
        // フェンスコードブロック内の `](` は無視される
        let md = "```\n[code](skip)\n```\n[link](url)";
        assert_eq!(find_next_link_candidate(md, 0), Some(26));
    }

    #[test]
    fn link_candidate_no_opening_bracket() {
        // `[` がない `](` は候補として返さない
        let md = "text](not-a-link)";
        assert_eq!(find_next_link_candidate(md, 0), None);
    }

    #[test]
    fn link_candidate_from_offset() {
        // 途中の位置から検索を開始する
        let md = "[a](x) [b](y)";
        assert_eq!(find_next_link_candidate(md, 6), Some(9));
    }

    #[test]
    fn link_candidate_double_backtick_inline_code() {
        // ダブルバッククォートのインラインコード内は無視される
        let md = "``[code](skip)`` [link](url)";
        assert_eq!(find_next_link_candidate(md, 0), Some(22));
    }

    #[test]
    fn link_candidate_unclosed_inline_code_is_literal() {
        let md = "`foo [link](url)";
        assert_eq!(find_next_link_candidate(md, 0), Some(10));
    }

    #[test]
    fn link_candidate_unclosed_fenced_code() {
        // 閉じられていないフェンスコードブロック内はリンク候補なし
        let md = "```\n[link](url)";
        assert_eq!(find_next_link_candidate(md, 0), None);
    }

    // --- has_matching_inline_code_closer テスト ---

    #[test]
    fn inline_code_closer_found() {
        // 同じ長さのバッククォート列が見つかる
        let md = "hello` world";
        assert!(has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_not_found() {
        // 閉じるバッククォート列がない
        let md = "hello world";
        assert!(!has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_length_mismatch() {
        // 長さが異なるバッククォート列は閉じ列として認識しない
        let md = "hello`` world";
        assert!(!has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_double_backtick() {
        // ダブルバッククォートの閉じ列
        let md = "code`` end";
        assert!(has_matching_inline_code_closer(md, 0, 2));
    }

    #[test]
    fn inline_code_closer_empty_input() {
        assert!(!has_matching_inline_code_closer("", 0, 1));
    }

    // --- マルチバイト文字を含むテーブル圧縮テスト ---

    #[test]
    fn compact_table_multibyte_cells() {
        // マルチバイト文字（日本語）を含むテーブルセルの圧縮
        let input = "|  名前  |  説明  |";
        assert_eq!(compact_markdown(input), "| 名前 | 説明 |");
    }

    #[test]
    fn compact_table_emoji_cells() {
        // 絵文字を含むテーブルセルの圧縮
        let input = "|  🚀 ロケット  |  ⭐ スター  |";
        assert_eq!(compact_markdown(input), "| 🚀 ロケット | ⭐ スター |");
    }

    // --- resolve_markdown_urls マルチバイトテスト ---

    #[test]
    fn resolve_link_with_multibyte_text() {
        // リンクテキストがマルチバイト文字
        let md = "[日本語テキスト](page.html)";
        let result = resolve_markdown_urls(md, "https://example.com/dir/");
        assert_eq!(
            result,
            "[日本語テキスト](https://example.com/dir/page.html)"
        );
    }

    #[test]
    fn resolve_image_with_multibyte_alt() {
        // 画像の alt テキストがマルチバイト文字
        let md = "![画像の説明](img.png)";
        let result = resolve_markdown_urls(md, "https://example.com/dir/");
        assert_eq!(result, "![画像の説明](https://example.com/dir/img.png)");
    }

    // --- find_link_close_paren 追加テスト ---

    #[test]
    fn find_close_paren_mixed_quotes_in_title() {
        // タイトル内にシングルクォートとダブルクォートが混在
        let s = r#"url "title with 'quotes'")"#;
        assert_eq!(find_link_close_paren(s), Some(s.len() - 1));
    }

    #[test]
    fn find_close_paren_unmatched_title_quote() {
        // 閉じられていないタイトルクォート内の ) は無視される
        let s = r#"url "title with ) inside")"#;
        assert_eq!(find_link_close_paren(s), Some(s.len() - 1));
    }

    // --- split_link_destination 追加テスト ---

    #[test]
    fn split_link_destination_angle_bracket_no_close() {
        // 閉じ `>` がない場合は標準形式として扱われる
        let (url, title, angle) = split_link_destination("<no-close");
        assert!(!angle);
        assert_eq!(url, "<no-close");
        assert_eq!(title, "");
    }

    #[test]
    fn split_link_destination_url_with_multibyte() {
        // マルチバイト文字を含む URL パス
        let (url, title, angle) = split_link_destination("/パス/ページ");
        assert_eq!(url, "/パス/ページ");
        assert_eq!(title, "");
        assert!(!angle);
    }

    // --- compact_markdown 境界テスト ---

    #[test]
    fn compact_table_pipe_only() {
        // パイプのみの行（長さ1）はテーブル行として扱わない
        let input = "|";
        assert_eq!(compact_markdown(input), "|");
    }

    #[test]
    fn compact_table_minimal_two_pipes() {
        // 2文字のパイプ列（最小テーブル行）
        let input = "||";
        assert_eq!(compact_markdown(input), "|  |");
    }

    // --- escape_js_string 境界テスト ---

    #[test]
    fn escape_js_string_multibyte() {
        // マルチバイト文字はそのまま出力される
        assert_eq!(escape_js_string("日本語"), r#""日本語""#);
    }

    #[test]
    fn escape_js_string_all_special_combined() {
        // すべての特殊文字を1つの文字列で組み合わせる
        let input = "\"\\\n\r\u{2028}\u{2029}";
        let expected = r#""\"\\\n\r\u2028\u2029""#;
        assert_eq!(escape_js_string(input), expected);
    }

    // --- CLI --ignore-date フラグのパース ---

    #[test]
    fn cli_ignore_date_flag() {
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "--ignore-date",
            "-o",
            "out.md",
        ])
        .unwrap();
        assert!(cli.ignore_date);
    }

    #[test]
    fn cli_ignore_date_default_false() {
        let cli = Cli::try_parse_from(["get-md", "https://example.com"]).unwrap();
        assert!(!cli.ignore_date);
    }

    // --- strip_dates 追加エッジケース ---

    #[test]
    fn strip_dates_removes_slash_datetime() {
        // スラッシュ区切りの日時
        assert_eq!(strip_dates("更新: 2024/03/15 14:30"), "更新: ");
    }

    #[test]
    fn strip_dates_no_dates_unchanged() {
        // 日時を含まない文字列はそのまま
        let input = "Hello, world! No dates here.";
        assert_eq!(strip_dates(input), input);
    }

    #[test]
    fn strip_dates_multiple_dates() {
        // 複数の日時パターンを一度に除去
        assert_eq!(strip_dates("from 2024-01-01 to 2024-12-31"), "from  to ");
    }

    #[test]
    fn strip_dates_iso8601_with_offset_4digit() {
        // 4桁オフセット (例: +0900)
        assert_eq!(strip_dates("time: 2024-03-15T09:00:00+0900"), "time: ");
    }

    // --- is_date_only_change 追加エッジケース ---

    #[test]
    fn date_only_change_non_utf8_returns_false() {
        // 非 UTF-8 バイト列は安全のため false を返す
        let old = &[0xFF, 0xFE];
        let new = &[0xFD, 0xFC];
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_mixed_date_formats() {
        // スラッシュとハイフン混在の日時変更
        let old = b"published: 2024-01-15 10:00";
        let new = b"published: 2024/06/20 15:30";
        assert!(is_date_only_change(old, new));
    }

    // --- find_link_close_paren 追加エッジケース ---

    #[test]
    fn find_close_paren_empty_title_quotes() {
        // 空のタイトルクォート
        assert_eq!(find_link_close_paren(r#"url "")  "#), Some(6));
    }

    #[test]
    fn find_close_paren_url_only_no_parens() {
        // 括弧なしの単純な URL
        assert_eq!(find_link_close_paren("https://example.com)"), Some(19));
    }

    // --- compact_markdown 追加エッジケース ---

    #[test]
    fn compact_table_row_single_pipe_not_table() {
        // パイプが1つだけの行はテーブルとして扱わない
        let input = "| not a table";
        assert_eq!(compact_markdown(input), "| not a table");
    }

    #[test]
    fn compact_table_trailing_content_after_pipe() {
        // パイプで終わらないがパイプで始まる行
        let input = "| cell | value";
        assert_eq!(compact_markdown(input), "| cell | value");
    }

    // --- resolve_markdown_urls 追加エッジケース ---

    #[test]
    fn resolve_multiple_images_on_same_line() {
        // 同一行に複数の画像リンク
        let md = "![a](img1.png) ![b](img2.png)";
        let result = resolve_markdown_urls(md, "https://example.com/page");
        assert_eq!(
            result,
            "![a](https://example.com/img1.png) ![b](https://example.com/img2.png)"
        );
    }

    #[test]
    fn resolve_link_with_escaped_bracket_in_text() {
        // リンクテキスト内のエスケープされた括弧
        let md = r"[text \] more](./path)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(result, r"[text \] more](https://example.com/path)");
    }

    #[test]
    fn resolve_hash_only_link_unchanged() {
        // フラグメントのみのリンクはベースURL + フラグメントに解決される
        let md = "[section](#heading)";
        let result = resolve_markdown_urls(md, "https://example.com/page");
        assert_eq!(result, "[section](https://example.com/page#heading)");
    }

    // --- split_link_destination 追加エッジケース ---

    #[test]
    fn split_link_destination_angle_bracket_with_space_in_url() {
        // 山括弧内のスペースを含むURL
        let (url, title, angle) = split_link_destination("<path with space>");
        assert_eq!(url, "path with space");
        assert_eq!(title, "");
        assert!(angle);
    }

    // --- has_matching_inline_code_closer 追加エッジケース ---

    #[test]
    fn inline_code_closer_triple_backtick() {
        // トリプルバッククォートの閉じ
        let md = "some ``` text ``` end";
        assert!(has_matching_inline_code_closer(md, 8, 3));
    }

    #[test]
    fn inline_code_closer_at_end_of_string() {
        // 文字列末尾にちょうど閉じバッククォートがある
        let md = "text `code`";
        assert!(has_matching_inline_code_closer(md, 6, 1));
    }

    // --- fence_marker 追加エッジケース ---

    #[test]
    fn fence_marker_tilde_two_not_enough() {
        assert_eq!(fence_marker("~~"), None);
    }

    #[test]
    fn fence_marker_mixed_chars_not_fence() {
        // バッククォートとチルダの混在はフェンスにならない
        assert_eq!(fence_marker("``~"), None);
    }

    // --- compact_markdown フェンス等号長テスト ---

    #[test]
    fn compact_fence_close_equal_length() {
        // 閉じフェンスが開きフェンスと同じ長さの場合にブロックが閉じること
        let input = "```\n| a | b |\n```\n| x | y |";
        let result = compact_markdown(input);
        // フェンス内のテーブル行はそのまま、フェンス外は圧縮される
        assert!(result.contains("| a | b |"));
        assert!(result.contains("| x | y |"));
    }

    // --- resolve_markdown_urls 空URL テスト ---

    #[test]
    fn resolve_empty_url_in_standard_link() {
        // [link]() の場合、URL が空なので変換せずそのまま
        let result = resolve_markdown_urls("[link]()", BASE);
        assert_eq!(result, "[link]()");
    }

    #[test]
    fn resolve_link_with_only_fragment_hash() {
        // [link](#) はフラグメントのみ — ベースURLにフラグメント付与
        let result = resolve_markdown_urls("[link](#)", BASE);
        assert!(result.contains("#"));
    }

    // --- find_link_close_paren タイトル内特殊文字 ---

    #[test]
    fn find_close_paren_title_with_angle_bracket() {
        // タイトルクォート内の < は特殊処理されない
        let result = find_link_close_paren(r#"url "title with <tag>")"#);
        assert!(result.is_some());
    }

    #[test]
    fn find_close_paren_title_with_paren_inside() {
        // タイトルクォート内の ) は閉じ括弧として扱わない
        let result = find_link_close_paren(r#"url "title (with) paren")"#);
        assert_eq!(result, Some(24));
    }

    // --- has_matching_inline_code_closer マルチバイトテスト ---

    #[test]
    fn inline_code_closer_with_multibyte_content() {
        // マルチバイト文字を含むインラインコード内でも閉じバッククォートを正しく検出
        let md = "`日本語テスト`";
        assert!(has_matching_inline_code_closer(md, 1, 1));
    }

    #[test]
    fn inline_code_closer_with_emoji_content() {
        // 絵文字を含むインラインコード内でも正しく検出
        let md = "`🎉🚀`rest";
        assert!(has_matching_inline_code_closer(md, 1, 1));
    }

    // --- resolve_markdown_urls 追加エッジケース ---

    #[test]
    fn resolve_link_immediately_after_fenced_code() {
        // フェンスコードブロック直後のリンクが正しく解決されること
        let input = "```\ncode\n```\n[link](./path)";
        let result = resolve_markdown_urls(input, BASE);
        assert!(result.contains("https://example.com/docs/en/path"));
    }

    #[test]
    fn resolve_multiple_inline_code_and_links() {
        // インラインコードとリンクが混在する場合
        let input = "`code` [link](./a) `more` [link2](./b)";
        let result = resolve_markdown_urls(input, BASE);
        assert!(result.contains("https://example.com/docs/en/a"));
        assert!(result.contains("https://example.com/docs/en/b"));
    }

    // --- compact_markdown 追加エッジケース ---

    #[test]
    fn compact_table_with_backtick_in_cell() {
        // セル内にバッククォートがあってもテーブルとして処理される
        let input = "| `code` | value |";
        let result = compact_markdown(input);
        assert_eq!(result, "| `code` | value |");
    }

    #[test]
    fn compact_markdown_trailing_newline_stripped_by_lines() {
        // str::lines() は末尾改行を含めないため、末尾改行は除去される
        let input = "| a | b |\n";
        let result = compact_markdown(input);
        assert_eq!(result, "| a | b |");
    }

    // --- find_next_link_candidate 追加テスト ---

    #[test]
    fn link_candidate_after_inline_code_with_bracket() {
        // インラインコード内の ]( は無視され、その後のリンクが見つかること
        let input = "`](` [real](url)";
        let result = find_next_link_candidate(input, 0);
        assert!(result.is_some());
        // ]( の位置はインラインコード外のもの
        let pos = result.unwrap();
        assert!(pos > 4); // インラインコード「`](`」の後
    }

    #[test]
    fn link_candidate_start_mid_line() {
        // 行の途中から検索開始した場合でもリンク候補を見つけること
        let input = "text [link](url) more";
        let result = find_next_link_candidate(input, 5);
        assert_eq!(result, Some(10));
    }

    // --- split_link_destination 追加テスト ---

    #[test]
    fn split_link_destination_angle_bracket_empty_title() {
        // 山括弧URL + 空タイトル
        let (url, title, angle) = split_link_destination("<https://example.com> ");
        assert_eq!(url, "https://example.com");
        assert_eq!(title, " ");
        assert!(angle);
    }

    // --- is_date_only_change 追加テスト ---

    #[test]
    fn date_only_change_empty_old_with_date_new() {
        // 空ファイル vs 日付のみの新コンテンツ
        // old に日時パターンが含まれないため false
        let old = b"";
        let new = b"2024-01-15";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_with_surrounding_text() {
        // 日付以外のテキストが異なれば false
        let old = b"created: 2024-01-15 by Alice";
        let new = b"created: 2024-06-20 by Bob";
        assert!(!is_date_only_change(old, new));
    }

    // --- split_link_destination 追加テスト ---

    #[test]
    fn split_link_destination_angle_bracket_escaped_gt() {
        // `\>` はエスケープされた `>` なので分割点にならない。
        // 次のエスケープされていない `>` で分割される。
        let (url, title, angle) = split_link_destination(r#"<path\>with> "title""#);
        assert!(angle);
        assert_eq!(url, r"path\>with");
        assert_eq!(title, r#" "title""#);
    }

    #[test]
    fn split_link_destination_standard_empty() {
        // 空文字列の場合
        let (url, title, angle) = split_link_destination("");
        assert!(!angle);
        assert_eq!(url, "");
        assert_eq!(title, "");
    }

    // --- resolve_markdown_urls 追加テスト ---

    #[test]
    fn resolve_query_only_relative_url() {
        // クエリのみの相対 URL はベース URL にクエリを付与する
        let md = "[search](?q=test)";
        let result = resolve_markdown_urls(md, BASE);
        assert_eq!(
            result,
            "[search](https://example.com/docs/en/page.md?q=test)"
        );
    }

    #[test]
    fn resolve_image_inside_link() {
        // 画像リンクがリンクテキスト内にある [![alt](img)](url)
        let md = "[![logo](./logo.png)](./home)";
        let result = resolve_markdown_urls(md, BASE);
        assert_eq!(
            result,
            "[![logo](https://example.com/docs/en/logo.png)](https://example.com/docs/en/home)"
        );
    }

    #[test]
    fn resolve_image_inside_link_with_unbalanced_base_parens() {
        // ネストした画像リンクでも、基準 URL 由来のアンバランスな括弧は山括弧形式で保護する
        let md = "[![logo](./logo.png)](./home)";
        let result = resolve_markdown_urls(md, "https://example.com/docs/(draft/");
        assert_eq!(
            result,
            "[![logo](<https://example.com/docs/(draft/logo.png>)](<https://example.com/docs/(draft/home>)"
        );
    }

    #[test]
    fn resolve_link_with_backslash_in_url() {
        // URL にバックスラッシュを含むリンク
        let md = r"[link](path%5Cfile)";
        let result = resolve_markdown_urls(md, BASE);
        assert_eq!(result, "[link](https://example.com/docs/en/path%5Cfile)");
    }

    // --- find_link_close_paren 追加テスト ---

    #[test]
    fn find_close_paren_trailing_backslash() {
        // 文字列末尾がバックスラッシュで終わる場合（閉じ括弧なし）
        assert_eq!(find_link_close_paren(r"url\"), None);
    }

    #[test]
    fn find_close_paren_multiple_nested_levels() {
        // 3段階のネストで最外の閉じ括弧を検出
        // depth: 1→2→3→2→1→0 (index 9)
        assert_eq!(find_link_close_paren("a(b(c)d)e)"), Some(9));
    }

    // --- is_date_only_change 追加テスト ---

    #[test]
    fn date_only_change_both_empty() {
        // 両方空の場合、old == new で false（同一内容は date-only change ではない）
        assert!(!is_date_only_change(b"", b""));
    }

    // --- strip_dates 追加テスト ---

    #[test]
    fn strip_dates_comma_fractional_seconds() {
        // ISO 8601 のカンマ区切り小数秒
        assert_eq!(
            strip_dates("event: 2024-03-15T14:30:45,678Z done"),
            "event:  done"
        );
    }

    #[test]
    fn strip_dates_preserves_non_date_numbers() {
        // 日付パターンに一致しない数字はそのまま残る
        let s = "version 1234 and count 99";
        assert_eq!(strip_dates(s), s);
    }

    // --- compact_markdown 追加テスト ---

    #[test]
    fn compact_multiple_tables_between_fences() {
        // フェンスコードブロック間に複数テーブルがある場合
        let input = "| a  | b  |\n| -- | -- |\n```\ncode\n```\n| c  | d  |\n| -- | -- |";
        let expected = "| a | b |\n| - | - |\n```\ncode\n```\n| c | d |\n| - | - |";
        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_markdown_single_line_no_newline() {
        // 改行なしの単一行入力
        assert_eq!(compact_markdown("just text"), "just text");
    }

    // --- find_next_link_candidate 追加テスト ---

    #[test]
    fn link_candidate_consecutive_fenced_blocks() {
        // 連続するフェンスコードブロックの後にリンク
        let md = "```\ncode1\n```\n```\ncode2\n```\n[link](url)";
        let candidate = find_next_link_candidate(md, 0);
        assert!(candidate.is_some());
        let pos = candidate.unwrap();
        assert_eq!(&md[pos..pos + 2], "](");
    }

    #[test]
    fn link_candidate_triple_backtick_in_inline_code() {
        // インラインコード内のトリプルバッククォートはフェンスとして扱わない
        let md = "text `` ``` `` [link](url)";
        let candidate = find_next_link_candidate(md, 0);
        assert!(candidate.is_some());
    }

    // --- has_matching_inline_code_closer 追加テスト ---

    #[test]
    fn inline_code_closer_longer_run_no_match() {
        // 長いバッククォート列は短い列のクローザーにならない
        assert!(!has_matching_inline_code_closer("text ```", 0, 1));
    }

    #[test]
    fn inline_code_closer_exact_match_after_content() {
        // コンテンツの後に正確な長さのクローザーがある
        assert!(has_matching_inline_code_closer("some code`` more", 0, 2));
    }

    // --- fence_marker 追加テスト ---

    #[test]
    fn fence_marker_only_backticks_long() {
        // 長いバッククォート列
        assert_eq!(fence_marker("``````"), Some(('`', 6)));
    }

    #[test]
    fn fence_marker_whitespace_only() {
        // 空白のみの行はフェンスではない
        assert_eq!(fence_marker("   "), None);
    }

    // --- escape_js_string 追加テスト ---

    #[test]
    fn escape_js_string_empty() {
        assert_eq!(escape_js_string(""), "\"\"");
    }

    #[test]
    fn escape_js_string_only_backslashes() {
        assert_eq!(escape_js_string(r"\\"), r#""\\\\""#);
    }

    // --- compact_table_row 追加テスト ---

    #[test]
    fn compact_table_row_separator_left_align() {
        // 左寄せセパレータ
        assert_eq!(compact_table_row("| :--- | ---- |"), "| :- | - |");
    }

    #[test]
    fn compact_table_row_separator_center_align() {
        // 中央寄せセパレータ
        assert_eq!(compact_table_row("| :---: | :--: |"), "| :-: | :-: |");
    }

    // --- is_date_only_change 修正に伴う追加テスト ---

    #[test]
    fn date_only_change_old_has_no_date_returns_false() {
        // old に日時パターンがない場合は date-only change ではない
        let old = b"hello world";
        let new = b"hello 2024-01-15 world";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_new_has_no_date_returns_false() {
        // new に日時パターンがない場合は date-only change ではない
        let old = b"hello 2024-01-15 world";
        let new = b"hello world";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_one_side_non_utf8_returns_false() {
        // 片方が非 UTF-8 の場合は false
        let old = b"data 2024-01-15";
        let new: &[u8] = &[0xFF, 0xFE, 0xFD];
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_both_have_dates_and_differ_only_in_dates() {
        // 双方に日時パターンがあり、日時以外が同一なら true
        let old = b"report 2024-01-15 10:00 final";
        let new = b"report 2025-03-20 14:30 final";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_dates_with_extra_text_diff_returns_false() {
        // 日時以外のテキストも異なる場合は false
        let old = b"report 2024-01-15 draft";
        let new = b"report 2025-03-20 final";
        assert!(!is_date_only_change(old, new));
    }

    // --- split_link_destination 修正に伴う追加テスト ---

    #[test]
    fn split_link_destination_angle_bracket_double_escaped_backslash_before_gt() {
        // `\\>` は偶数バックスラッシュ後の `>` なので分割点になる
        let (url, title, angle) = split_link_destination(r#"<path\\> "title""#);
        assert!(angle);
        assert_eq!(url, r"path\\");
        assert_eq!(title, r#" "title""#);
    }

    #[test]
    fn split_link_destination_angle_bracket_no_unescaped_gt() {
        // エスケープされた `>` のみで閉じ `>` がない場合は山括弧形式として扱わない
        let (url, title, angle) = split_link_destination(r"<path\>");
        assert!(!angle);
        assert_eq!(url, r"<path\>");
        assert_eq!(title, "");
    }

    #[test]
    fn split_link_destination_angle_bracket_multiple_escaped_gt() {
        // 複数の `\>` をスキップし、最初のエスケープされていない `>` で分割
        let (url, title, angle) = split_link_destination(r#"<a\>b\>c> "t""#);
        assert!(angle);
        assert_eq!(url, r"a\>b\>c");
        assert_eq!(title, r#" "t""#);
    }

    // --- resolve_markdown_urls 修正（山括弧エスケープ）に伴うテスト ---

    #[test]
    fn resolve_angle_bracket_url_with_escaped_gt() {
        // 山括弧内のエスケープされた `>` を含む URL が正しく解決される
        let base = "https://example.com/docs/";
        let md = r"[link](<path\>file>)";
        let result = resolve_markdown_urls(md, base);
        assert_eq!(result, "[link](<https://example.com/docs/path%3Efile>)");
    }

    // --- テストカバレッジ補強 ---

    #[test]
    fn compact_markdown_crlf_normalized_to_lf() {
        // CRLF 入力は LF に正規化される
        let input = "| a  | b  |\r\n|---|---|\r\n| 1  | 2  |";
        let result = compact_markdown(input);
        assert!(!result.contains('\r'));
        assert_eq!(result, "| a | b |\n| - | - |\n| 1 | 2 |");
    }

    #[test]
    fn resolve_empty_link_text() {
        // 空のリンクテキスト `[](url)` でも URL が解決される
        let result = resolve_markdown_urls("[](./page)", BASE);
        assert_eq!(result, "[](https://example.com/docs/en/page)");
    }

    #[test]
    fn resolve_link_with_newline_between_links() {
        // 改行を挟んだ複数リンクが両方とも解決される
        let input = "[a](./x)\n\n[b](./y)";
        let result = resolve_markdown_urls(input, BASE);
        assert!(result.contains("example.com/docs/en/x"));
        assert!(result.contains("example.com/docs/en/y"));
    }

    #[test]
    fn is_date_only_change_identical_dates_different_text() {
        // 日時は同じだが他のテキストが異なる場合は date-only change ではない
        let old = b"Updated: 2025-01-01 12:00 - version A";
        let new = b"Updated: 2025-01-01 12:00 - version B";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn compact_markdown_only_table_rows() {
        // テーブル行のみの入力でも正常に処理される
        let input = "| a  | b  |\n| c  | d  |";
        assert_eq!(compact_markdown(input), "| a | b |\n| c | d |");
    }

    #[test]
    fn compact_markdown_empty_fenced_code_block() {
        // 空のフェンスブロックの直後にテーブルがある場合
        let input = "```\n```\n| a  | b  |";
        assert_eq!(compact_markdown(input), "```\n```\n| a | b |");
    }

    #[test]
    fn find_close_paren_deeply_nested_three_levels() {
        // 3 段階ネストの括弧を正しく処理する（暗黙の開き括弧を含むため +1 レベル）
        let s = "a(b(c)))";
        assert_eq!(find_link_close_paren(s), Some(7));
    }

    #[test]
    fn split_unescaped_table_cells_empty_input() {
        // 空文字列の入力では空の 1 セルが返る
        let cells = split_unescaped_table_cells("");
        assert_eq!(cells, vec![""]);
    }

    #[test]
    fn escape_js_string_with_form_feed_and_backspace() {
        // フォームフィードとバックスペースはそのまま通過する
        let result = escape_js_string("a\x08b\x0cc");
        assert_eq!(result, "\"a\x08b\x0cc\"");
    }

    #[test]
    fn resolve_link_with_only_whitespace_text() {
        // 空白のみのリンクテキストでも URL が解決される
        let result = resolve_markdown_urls("[ ](./page)", BASE);
        assert_eq!(result, "[ ](https://example.com/docs/en/page)");
    }

    #[test]
    fn compact_table_separator_right_align() {
        // 右寄せセパレータの配置が保持される
        let result = compact_table_row("|-------:|");
        assert_eq!(result, "| -: |");
    }

    #[test]
    fn fence_marker_four_backticks_with_lang() {
        // 4 個以上のバッククォート + 言語指定
        let (marker, len) = fence_marker("````rust").unwrap();
        assert_eq!(marker, '`');
        assert_eq!(len, 4);
    }

    #[test]
    fn compact_adjacent_fenced_blocks_different_markers() {
        // バッククォートとチルダの異なるフェンスブロックが隣接する場合
        let input = "```\ncode1\n```\n~~~\ncode2\n~~~\n| a  | b  |";
        let result = compact_markdown(input);
        assert!(result.contains("code1"));
        assert!(result.contains("code2"));
        assert!(result.ends_with("| a | b |"));
    }

    #[test]
    fn resolve_link_base_url_with_fragment() {
        // ベース URL にフラグメントがある場合でも正しく解決される
        let result = resolve_markdown_urls("[a](./page)", "https://example.com/docs/#section");
        assert_eq!(result, "[a](https://example.com/docs/page)");
    }

    #[test]
    fn split_link_destination_standard_url_with_paren() {
        // 標準形式で URL に括弧を含まない場合のタイトル分割
        let (url, title, angle) = split_link_destination("url \"title\"");
        assert!(!angle);
        assert_eq!(url, "url");
        assert_eq!(title, " \"title\"");
    }

    #[test]
    fn is_date_only_change_single_date_in_large_text() {
        // 大きなテキスト中の一箇所だけ日時が変わった場合
        let old = b"Header\nContent line 1\nDate: 2025-01-01 10:00\nContent line 2\nFooter";
        let new = b"Header\nContent line 1\nDate: 2025-06-15 14:30\nContent line 2\nFooter";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn compact_table_row_unicode_alignment_markers() {
        // Unicode を含むセパレータ行は通常のセパレータとして処理される
        let result = compact_table_row("|:---:|:---:|");
        assert_eq!(result, "| :-: | :-: |");
    }

    #[test]
    fn find_next_link_candidate_skips_nested_fenced_code() {
        // 4 つのバッククォートで開いたフェンスは 3 つでは閉じない
        let md = "````\n[a](./b)\n```\n[c](./d)\n````\n[e](./f)";
        let candidate = find_next_link_candidate(md, 0);
        // フェンス内の `](` は無視され、フェンス外の `](` が見つかる
        assert!(candidate.is_some());
        let pos = candidate.unwrap();
        let after = &md[pos..];
        assert!(after.starts_with("](./f)"));
    }

    #[test]
    fn idle_browser_timeout_typical_value() {
        // 一般的な 60 秒タイムアウトのバッファ確認
        assert_eq!(idle_browser_timeout(60), Duration::from_secs(90));
    }

    // --- split_link_destination の追加テスト ---

    #[test]
    fn split_link_destination_empty_input() {
        // 空文字列は URL なし・タイトルなし
        let (url, title, angle) = split_link_destination("");
        assert_eq!(url, "");
        assert_eq!(title, "");
        assert!(!angle);
    }

    #[test]
    fn split_link_destination_angle_bracket_unclosed_falls_to_standard() {
        // 閉じ `>` がない山括弧は標準形式としてパースされる
        let (url, title, angle) = split_link_destination("<no-close");
        assert_eq!(url, "<no-close");
        assert_eq!(title, "");
        assert!(!angle);
    }

    #[test]
    fn split_link_destination_standard_only_whitespace() {
        // 空白のみの入力は URL が空
        let (url, title, angle) = split_link_destination("   ");
        assert_eq!(url, "");
        assert_eq!(title, "   ");
        assert!(!angle);
    }

    // --- find_link_close_paren の追加テスト ---

    #[test]
    fn find_close_paren_only_whitespace() {
        // 空白だけで閉じ括弧がない場合は None
        assert_eq!(find_link_close_paren("   "), None);
    }

    #[test]
    fn find_close_paren_immediate_close() {
        // 開き括弧直後に閉じ括弧
        assert_eq!(find_link_close_paren(")"), Some(0));
    }

    #[test]
    fn find_close_paren_backslash_at_end_without_close() {
        // 末尾にバックスラッシュがあり閉じ括弧がない
        assert_eq!(find_link_close_paren("url\\"), None);
    }

    // --- compact_markdown の追加テスト ---

    #[test]
    fn compact_tilde_fence_inside_backtick_fence() {
        // バッククォートフェンス内のチルダ行はフェンスとして扱わない
        let md = "```\n~~~\n| a | b |\n~~~\n```\n|  c  |  d  |";
        let result = compact_markdown(md);
        // フェンス内のテーブル行はそのまま保持
        assert!(result.contains("| a | b |"));
        // フェンス外のテーブル行は圧縮される
        assert!(result.contains("| c | d |"));
    }

    #[test]
    fn compact_markdown_only_pipes() {
        // パイプだけの行（長さ 1）はテーブルとして扱わない
        assert_eq!(compact_markdown("|"), "|");
    }

    #[test]
    fn compact_markdown_consecutive_tables() {
        // 連続するテーブルが両方とも圧縮される
        let md = "|  a  |  b  |\n|  c  |  d  |";
        let result = compact_markdown(md);
        assert_eq!(result, "| a | b |\n| c | d |");
    }

    // --- escape_js_string の追加テスト ---

    #[test]
    fn escape_js_string_template_literal_backtick() {
        // テンプレートリテラルのバッククォートはそのまま通過
        assert_eq!(escape_js_string("`template`"), "\"`template`\"");
    }

    #[test]
    fn escape_js_string_surrogate_boundary() {
        // U+FFFF の非BMP境界付近の文字がそのまま通過する
        let s = "\u{FEFF}"; // BOM
        let result = escape_js_string(s);
        assert_eq!(result, format!("\"{}\"", s));
    }

    // --- resolve_markdown_urls の追加テスト ---

    #[test]
    fn resolve_url_with_path_base() {
        // ベースURLにパスがある場合の相対URL解決
        let md = "[link](other.html)";
        let result = resolve_markdown_urls(md, "https://example.com/dir/page.html");
        assert_eq!(result, "[link](https://example.com/dir/other.html)");
    }

    #[test]
    fn resolve_url_preserves_non_link_brackets() {
        // `]` と `(` が離れている場合はリンクとして扱わない
        let md = "array[0] = (value)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(result, md);
    }

    #[test]
    fn resolve_consecutive_angle_bracket_links() {
        // 連続する山括弧リンクが両方とも解決される
        let md = "[a](<./x>) [b](<./y>)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(
            result,
            "[a](<https://example.com/x>) [b](<https://example.com/y>)"
        );
    }

    // --- find_next_link_candidate の追加テスト ---

    #[test]
    fn link_candidate_at_end_of_string() {
        // 文字列末尾の `](` は見つかる
        let md = "[a](";
        let candidate = find_next_link_candidate(md, 0);
        assert_eq!(candidate, Some(2));
    }

    #[test]
    fn link_candidate_start_at_last_char() {
        // 開始位置が最後の文字の場合は None
        let md = "abc";
        assert_eq!(find_next_link_candidate(md, 2), None);
    }

    #[test]
    fn link_candidate_start_at_newline() {
        // 開始位置が改行文字の場合、次の行がフェンスかどうか正しく判定
        let md = "text\n```\n[a](b)\n```\n[c](d)";
        let pos = find_next_link_candidate(md, 4);
        assert!(pos.is_some());
        let after = &md[pos.unwrap()..];
        assert!(after.starts_with("](d)"));
    }

    // --- is_escaped_markdown_char の追加テスト ---

    #[test]
    fn escaped_char_at_string_boundary() {
        // idx が 0 の場合はバックスラッシュ無し → false
        assert!(!is_escaped_markdown_char("[text", 0));
    }

    #[test]
    fn escaped_char_four_backslashes() {
        // 4 つのバックスラッシュ → 偶数 → エスケープされていない
        assert!(!is_escaped_markdown_char("\\\\\\\\[", 4));
    }

    // --- compact_table_row の追加テスト ---

    #[test]
    fn compact_table_row_all_separator() {
        // 全セルがセパレータの場合
        let result = compact_table_row("| --- | --- | --- |");
        assert_eq!(result, "| - | - | - |");
    }

    #[test]
    fn compact_table_row_mixed_content_and_separator() {
        // セパレータと通常セルの混在（通常のテーブルヘッダ+セパレータ）
        let result = compact_table_row("|  Header  |  Value  |");
        assert_eq!(result, "| Header | Value |");
    }

    // --- is_date_only_change の追加テスト ---

    #[test]
    fn date_only_change_multiline_with_dates() {
        // 複数行で各行に日付がある場合
        let old = b"line1 2024-01-01\nline2 2024-01-01 10:00";
        let new = b"line1 2025-12-31\nline2 2025-12-31 23:59";
        assert!(is_date_only_change(old, new));
    }

    #[test]
    fn date_only_change_date_in_url_path() {
        // URL パス内の日付パターンも日付として扱われる
        let old = b"/blog/2024-01-01/post";
        let new = b"/blog/2025-06-15/post";
        assert!(is_date_only_change(old, new));
    }

    // --- strip_dates の追加テスト ---

    #[test]
    fn strip_dates_iso8601_with_negative_offset() {
        // 負のタイムゾーンオフセット
        assert_eq!(strip_dates("2024-01-01T10:00:00-05:00"), "");
    }

    #[test]
    fn strip_dates_date_at_start_and_end() {
        // 文字列の先頭と末尾に日付
        assert_eq!(strip_dates("2024-01-01 text 2024-12-31"), " text ");
    }

    // --- has_matching_inline_code_closer の追加テスト ---

    #[test]
    fn inline_code_closer_from_end_of_string() {
        // 開始位置が文字列末尾 → 閉じが見つからない
        let md = "`code`";
        assert!(!has_matching_inline_code_closer(md, md.len(), 1));
    }

    #[test]
    fn inline_code_closer_multibyte_between_backticks() {
        // マルチバイト文字を含むインラインコード
        let md = "``日本語のコード``";
        assert!(has_matching_inline_code_closer(md, 2, 2));
    }

    // --- resolve_markdown_urls: base.join() Err 分岐 ---

    #[test]
    fn resolve_angle_bracket_url_join_resolves_colon_path() {
        // コロンを含むパスは base.join() が成功し、絶対 URL に解決される
        let md = "[link](<:invalid:url>)";
        let result = resolve_markdown_urls(md, BASE);
        assert_eq!(result, "[link](<https://example.com/docs/en/:invalid:url>)");
    }

    #[test]
    fn resolve_standard_url_join_resolves_colon_path() {
        // コロンを含むパスは base.join() が成功し、絶対 URL に解決される
        let md = "[link](:invalid:url)";
        let result = resolve_markdown_urls(md, BASE);
        assert_eq!(result, "[link](https://example.com/docs/en/:invalid:url)");
    }

    // --- find_link_close_paren: タイトル内のエスケープ済み引用符 ---

    #[test]
    fn find_close_paren_escaped_quote_in_title() {
        // タイトル内のエスケープされた引用符はタイトル終端とみなさない
        let input = r#"url "title with \" inside")"#;
        let result = find_link_close_paren(input);
        assert_eq!(result, Some(input.len() - 1));
    }

    #[test]
    fn find_close_paren_escaped_single_quote_in_title() {
        // シングルクォートのタイトル内でもエスケープが有効
        let input = r"url 'title with \' inside')";
        let result = find_link_close_paren(input);
        assert_eq!(result, Some(input.len() - 1));
    }

    // --- find_link_close_paren: 未閉鎖のタイトル引用符 ---

    #[test]
    fn find_close_paren_unclosed_title_quote_returns_none() {
        // タイトルの引用符が閉じられない → 閉じ括弧が見つからず None
        let result = find_link_close_paren(r#"url "unclosed title)"#);
        assert_eq!(result, None);
    }

    // --- find_link_close_paren: depth > 1 では引用符をタイトルとして扱わない ---

    #[test]
    fn find_close_paren_quote_at_depth_two_not_title() {
        // ネストした括弧内の引用符はタイトル開始として扱わない
        let input = r#"(a "b") "title")"#;
        let result = find_link_close_paren(input);
        // (a "b") で depth が 1 に戻り、その後 "title" がタイトルとして扱われ、
        // 最後の ) で depth 0 → Some
        assert_eq!(result, Some(input.len() - 1));
    }

    // --- compact_markdown: CRLF を含むフェンスブロック ---

    #[test]
    fn compact_fenced_code_block_with_crlf() {
        // フェンスブロック内の CRLF 行は変換せず保持
        let input = "```\r\n| a | b |\r\n```\r\n| c  | d  |";
        let result = compact_markdown(input);
        assert!(result.contains("| a | b |")); // フェンス内は圧縮しない
        assert!(result.contains("|c|d|") || result.contains("| c | d |"));
    }

    // --- compact_table_row: コロンのみのセパレータセル ---

    #[test]
    fn compact_table_separator_colon_only() {
        // コロンのみのセル（ダッシュなし）もセパレータとして扱われる
        // `:` → starts_with(':') かつ ends_with(':') なので `:-:`
        assert_eq!(compact_table_row("| : | :: |"), "| :-: | :-: |");
    }

    // --- split_link_destination: 山括弧内の末尾バックスラッシュ ---

    #[test]
    fn split_link_destination_angle_bracket_trailing_backslash() {
        // 山括弧が閉じず末尾がバックスラッシュ → 標準形式にフォールバック
        let (url, title, angle) = split_link_destination(r"<path\\");
        assert!(!angle);
        assert_eq!(url, r"<path\\");
        assert_eq!(title, "");
    }

    // --- find_next_link_candidate: 行途中からの開始でフェンスマーカーをスキップ ---

    #[test]
    fn link_candidate_mid_line_start_treats_backticks_as_inline_code() {
        // 行の途中から開始した場合、``` はフェンスではなくインラインコードとして扱われる。
        // インラインコードが [link](url) を包含するため、リンクは検出されない。
        let md = "text ```\n[link](url)\n```";
        let result = find_next_link_candidate(md, 5);
        assert!(result.is_none());
    }

    // --- find_next_link_candidate: 未閉鎖の長いバッククォート列 ---

    #[test]
    fn link_candidate_unclosed_double_backtick_is_literal() {
        // 閉じられないダブルバッククォートはリテラルとして扱い、リンクを検出
        let md = "``unclosed [link](url)";
        let result = find_next_link_candidate(md, 0);
        assert!(result.is_some());
    }

    // --- resolve_markdown_urls: 山括弧内が空白のみの URL ---

    #[test]
    fn resolve_angle_bracket_whitespace_only_url() {
        // 山括弧内が空白のみの URL
        let md = "[link](<  >)";
        let result = resolve_markdown_urls(md, BASE);
        // 空白のみの URL は base.join が処理し、ベース URL 自体に解決される
        assert!(result.starts_with("[link](<"));
        assert!(result.ends_with(">)"));
    }

    // --- find_link_close_paren: エスケープ済み `(` と通常の `()` の混在 ---

    #[test]
    fn find_close_paren_escaped_open_with_nested_parens() {
        // エスケープされた `\(` は depth を増やさず、通常の `()` は正しくネスト
        let input = r"a\((b))";
        let result = find_link_close_paren(input);
        // \( はスキップ、(b) で depth 2→1、最後の ) で depth 1→0
        assert_eq!(result, Some(input.len() - 1));
    }

    // --- compact_markdown: 先頭に空白があるテーブル行 ---

    #[test]
    fn compact_table_row_with_leading_whitespace() {
        // 先頭に空白があるテーブル行は trim 後にパイプ判定される
        let input = "  | col1  | col2  |";
        let result = compact_markdown(input);
        // trim() で先頭空白が除去され、テーブル行として圧縮される
        assert_eq!(result, "| col1 | col2 |");
    }

    // --- is_date_only_change: 日時除去後に空白パターンが異なる ---

    #[test]
    fn date_only_change_whitespace_diff_after_strip_returns_false() {
        // 日時を除去した後の空白パターンが異なる → date-only ではない
        let old = b"A 2024-01-01 B";
        let new = b"A  2024-02-02 B";
        assert!(!is_date_only_change(old, new));
    }

    // --- find_link_close_paren: 山括弧内のエスケープ済み `>` ---

    #[test]
    fn find_close_paren_escaped_gt_then_real_gt_in_angle_dest() {
        // 山括弧内で \> はスキップし、次の > で山括弧を閉じる
        let input = r"<path\>file> rest)";
        let result = find_link_close_paren(input);
        assert_eq!(result, Some(input.len() - 1));
    }

    // --- compact_table_row: 追加エッジケース ---

    #[test]
    fn compact_table_row_separator_no_dashes_only_colons() {
        // コロンのみ（::）は先頭・末尾ともにコロン → 中央揃え扱い
        assert_eq!(compact_table_row("|::|"), "| :-: |");
    }

    #[test]
    fn compact_table_row_wide_padding_cells() {
        // 大量の余白がある行を圧縮
        assert_eq!(compact_table_row("|   foo   |   bar   |"), "| foo | bar |");
    }

    #[test]
    fn compact_table_row_escaped_pipe_in_content() {
        // エスケープ済みパイプはセル区切りとして扱わない
        assert_eq!(compact_table_row(r"| a\|b | c |"), r"| a\|b | c |");
    }

    // --- idle_browser_timeout: 追加エッジケース ---

    #[test]
    fn idle_browser_timeout_large_value() {
        // 大きいが溢れない値
        let d = idle_browser_timeout(1000);
        assert_eq!(d, Duration::from_secs(1030));
    }

    #[test]
    fn idle_browser_timeout_max_minus_30() {
        // u64::MAX - 30 は加算後にちょうど u64::MAX
        let d = idle_browser_timeout(u64::MAX - 30);
        assert_eq!(d, Duration::from_secs(u64::MAX));
    }

    // --- file_status: 追加エッジケース ---

    #[test]
    fn file_status_old_equals_new_no_git() {
        // 既存内容と新内容が同一で git 管理外の場合は unchanged
        let dir = std::env::temp_dir().join("get_md_test_fs_eq");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("same.txt");
        let content = b"identical content";
        std::fs::write(&path, content).unwrap();
        let old = Some(content.to_vec());
        let (icon, status) = file_status(&path, true, &old, content, false);
        // git 管理外なので has_unstaged_changes は false → unchanged
        assert_eq!(icon, "✔");
        assert_eq!(status, "unchanged");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn file_status_old_differs_from_new() {
        // 既存内容と新内容が異なれば updated
        let path = Path::new("/tmp/get_md_test_diff.txt");
        let old = Some(b"old".to_vec());
        let (icon, status) = file_status(path, true, &old, b"new", false);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");
    }

    // --- escape_js_string: 追加エッジケース ---

    #[test]
    fn escape_js_string_tab_preserved() {
        // タブ文字はそのまま通過する（CSS セレクタには通常含まれないが安全）
        assert_eq!(escape_js_string("\t"), "\"\t\"");
    }

    #[test]
    fn escape_js_string_mixed_newlines() {
        // \r\n の各文字が個別にエスケープされる
        assert_eq!(escape_js_string("a\r\nb"), r#""a\r\nb""#);
    }

    #[test]
    fn escape_js_string_backslash_before_quote() {
        // バックスラッシュとクォートの連続が正しくエスケープされる
        assert_eq!(escape_js_string(r#"a\"b"#), r#""a\\\"b""#);
    }

    // --- compact_markdown: 追加エッジケース ---

    #[test]
    fn compact_markdown_indented_table_between_text() {
        // テキストの間にあるインデントなしテーブルが圧縮される
        let input = "text\n|  a  |  b  |\n| --- | --- |\n|  1  |  2  |\nmore text";
        let expected = "text\n| a | b |\n| - | - |\n| 1 | 2 |\nmore text";
        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_markdown_empty_fence_tilde() {
        // チルダの空フェンスブロック内のテーブル行は変更しない
        let input = "~~~\n|  padded  |\n~~~";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_markdown_nested_fence_same_char() {
        // 外側のフェンスが長い場合、短いフェンスマーカーは閉じとして扱わない
        let input = "````\n```\n|  x  |\n```\n````";
        assert_eq!(compact_markdown(input), input);
    }

    // --- resolve_markdown_urls: 追加エッジケース ---

    #[test]
    fn resolve_url_with_double_encoded_space() {
        // %20 を含むURLがそのまま解決される
        let md = "[link](path%20with%20space)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(result, "[link](https://example.com/path%20with%20space)");
    }

    #[test]
    fn resolve_url_with_consecutive_dots() {
        // ../../ のような多段の相対パス
        let md = "[link](../../page)";
        let result = resolve_markdown_urls(md, "https://example.com/a/b/c/");
        assert_eq!(result, "[link](https://example.com/a/page)");
    }

    #[test]
    fn resolve_url_preserves_trailing_slash() {
        // 末尾スラッシュが保持される
        let md = "[link](dir/)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(result, "[link](https://example.com/dir/)");
    }

    #[test]
    fn resolve_angle_bracket_url_with_multiple_escaped_gt() {
        // 山括弧内に複数のエスケープ済み > がある場合、URL 解決後も山括弧で囲まれる
        let md = r"[link](<a\>b\>c>)";
        let result = resolve_markdown_urls(md, "https://example.com/");
        assert_eq!(result, "[link](<https://example.com/a%3Eb%3Ec>)");
    }

    #[test]
    fn resolve_empty_base_url_returns_unchanged() {
        // 空のベースURLではパースエラーとなり変換なし
        let md = "[link](./page)";
        assert_eq!(resolve_markdown_urls(md, ""), md);
    }

    // --- find_next_link_candidate: 追加エッジケース ---

    #[test]
    fn link_candidate_multiple_links_finds_first() {
        // 複数リンクがある場合、最初のものを返す
        let md = "[a](url1) [b](url2)";
        let pos = find_next_link_candidate(md, 0);
        assert_eq!(pos, Some(2)); // 最初の ](
    }

    #[test]
    fn link_candidate_after_first_link() {
        // 最初のリンクを飛ばして2番目のリンクを検出
        let md = "[a](url1) [b](url2)";
        let pos = find_next_link_candidate(md, 3);
        assert_eq!(pos, Some(12)); // 2番目の ](
    }

    #[test]
    fn link_candidate_fence_at_end_of_input() {
        // 入力末尾のフェンスブロック（改行なし）
        let md = "text\n```\ncode";
        let pos = find_next_link_candidate(md, 0);
        assert_eq!(pos, None);
    }

    // --- has_matching_inline_code_closer: 追加エッジケース ---

    #[test]
    fn inline_code_closer_adjacent_different_lengths() {
        // 異なる長さのバッククォート列が隣接している場合
        let md = "``x```"; // `` の閉じは見つからない（``` は長さ3で不一致）
        assert!(!has_matching_inline_code_closer(md, 2, 2));
    }

    #[test]
    fn inline_code_closer_separated_by_multibyte() {
        // マルチバイト文字を挟んだバッククォート閉じ
        let md = "あ`";
        assert!(has_matching_inline_code_closer(md, 0, 1));
    }

    // --- split_link_destination: 追加エッジケース ---

    #[test]
    fn split_link_destination_standard_with_multiple_spaces() {
        // 最初のエスケープされていない空白でタイトルと分離
        let (url, title, angle) = split_link_destination("url first second");
        assert_eq!(url, "url");
        assert_eq!(title, " first second");
        assert!(!angle);
    }

    #[test]
    fn split_link_destination_angle_bracket_with_backslash_at_end() {
        // `\>` はエスケープ済み → 閉じ山括弧が見つからず標準形式にフォールバック
        let (url, title, angle) = split_link_destination(r"<url\>");
        assert_eq!(url, r"<url\>");
        assert_eq!(title, "");
        assert!(!angle);
    }

    // --- is_escaped_markdown_char: 追加エッジケース ---

    #[test]
    fn escaped_char_idx_zero() {
        // 先頭位置はエスケープされない
        assert!(!is_escaped_markdown_char("[link", 0));
    }

    #[test]
    fn escaped_char_five_backslashes() {
        // 奇数個（5個）のバックスラッシュ → エスケープ
        assert!(is_escaped_markdown_char(r"\\\\\[", 5));
    }

    // --- find_link_close_paren: 追加エッジケース ---

    #[test]
    fn find_close_paren_title_with_escaped_quote_then_close() {
        // タイトル内にエスケープされた引用符があり、その後に閉じ括弧
        let input = r#"url "title with \" inside")"#;
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    #[test]
    fn find_close_paren_deeply_nested_four_levels() {
        // 暗黙の開き括弧 + 3段ネスト → 閉じ4個で depth=0
        let input = "a(b(c(d))))";
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    #[test]
    fn find_close_paren_angle_dest_with_paren_inside() {
        // 山括弧内の括弧は無視される
        let input = "<url(with)paren> \"title\")";
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    #[test]
    fn find_close_paren_unclosed_angle_destination_returns_none() {
        // 閉じ `>` がない山括弧リンク先では、後続の `)` をリンク終端として扱わない
        let input = "<broken [next](./ok)";
        assert_eq!(find_link_close_paren(input), None);
    }

    // --- strip_dates: 追加エッジケース ---

    #[test]
    fn strip_dates_only_date_becomes_empty() {
        // 日付だけの文字列は空になる
        assert_eq!(strip_dates("2024-01-01"), "");
    }

    #[test]
    fn strip_dates_iso8601_with_comma_and_offset() {
        // カンマ区切りの小数秒とオフセット
        assert_eq!(
            strip_dates("prefix 2024-01-01T12:00:00,123+09:00 suffix"),
            "prefix  suffix"
        );
    }

    // --- fence_marker: 追加エッジケース ---

    #[test]
    fn fence_marker_exactly_three_tildes() {
        assert_eq!(fence_marker("~~~"), Some(('~', 3)));
    }

    #[test]
    fn fence_marker_backtick_with_spaces_after_info() {
        // 情報文字列の後にスペースがある場合
        assert_eq!(fence_marker("``` rust "), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_leading_non_marker_char() {
        // 先頭がマーカー文字でない場合
        assert_eq!(fence_marker("a```"), None);
    }

    // --- file_status: 実フロー再現テスト ---

    #[test]
    fn file_status_new_file_after_creation() {
        // バグ再現: File::create でファイルを作成した後でも
        // file_existed_before=false なら "created" を返すこと
        let dir = std::env::temp_dir().join(format!(
            "get-md-fs-new-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("brand_new.md");

        // ファイルは存在しない → file_existed_before=false
        assert!(!path.exists());
        let content = b"new content";

        // File::create でファイルを作成する（実プログラムと同じ流れ）
        std::fs::write(&path, content).unwrap();
        assert!(path.exists()); // 作成後はファイルが存在する

        // file_existed_before=false で呼べば "created" になること
        let (icon, status) = file_status(&path, false, &None, content, false);
        assert_eq!(icon, "✨");
        assert_eq!(status, "created");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_status_overwrite_existing_file() {
        // 既存ファイルを上書きした場合は "updated" を返すこと
        let dir = std::env::temp_dir().join(format!(
            "get-md-fs-overwrite-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("existing.md");

        let old = b"old content";
        std::fs::write(&path, old).unwrap();

        let new = b"new content";
        std::fs::write(&path, new).unwrap();

        let (icon, status) = file_status(&path, true, &Some(old.to_vec()), new, false);
        assert_eq!(icon, "📝");
        assert_eq!(status, "updated");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- strip_dates: 境界パターンテスト ---

    #[test]
    fn strip_dates_invalid_month_day_still_matches() {
        // 正規表現は値の妥当性を検証しない（意図的な仕様）
        assert_eq!(strip_dates("2024-99-99"), "");
    }

    #[test]
    fn strip_dates_adjacent_digits_boundary() {
        // 日付パターンの前後に数字がある場合、正規表現のバウンダリ動作を確認
        let result = strip_dates("id12024-01-01234");
        // \d{4} が "2024" にマッチし、残りは "id1" + "234"
        assert_eq!(result, "id1234");
    }

    // --- find_link_close_paren: 深いネスト + 引用符 ---

    #[test]
    fn find_close_paren_depth_three_with_quotes() {
        // depth=3 で引用符が出現してもタイトルとして扱わない
        // ]( → depth=1, '(' → 2, '(' → 3, '"..."' はタイトルではない, ')' → 2, ')' → 1, ')' → 0
        let input = r#"a(b("not-title")c))"#;
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    #[test]
    fn find_close_paren_depth_two_quote_with_close_paren_inside() {
        // depth=2 では引用符はタイトルにならないため、中の ')' は通常のネスト閉じ
        let input = r#"a("b)c)"#;
        // depth=1: 'a', '(' → depth=2, '"' は depth>1 でタイトルにならない,
        // 'b', ')' → depth=1, 'c', ')' → depth=0
        assert_eq!(find_link_close_paren(input), Some(input.len() - 1));
    }

    // --- テストカバレッジ補完: 画像リンクの alt 空テスト ---

    #[test]
    fn resolve_empty_alt_image_url() {
        // alt テキストなしの画像 ![](url) でも URL 解決される
        assert_eq!(
            resolve_markdown_urls("![](./img.png)", BASE),
            "![](https://example.com/docs/en/img.png)",
        );
    }

    #[test]
    fn resolve_image_empty_alt_with_title() {
        // alt なし＋タイトル付き画像の URL 解決
        assert_eq!(
            resolve_markdown_urls(r#"![](./pic.png "photo")"#, BASE),
            r#"![](https://example.com/docs/en/pic.png "photo")"#,
        );
    }

    // --- テストカバレッジ補完: 日時比較の境界テスト ---

    #[test]
    fn date_only_change_both_non_utf8_returns_false() {
        // 双方が非 UTF-8 の場合は安全のため false
        let old: &[u8] = &[0xFF, 0xFE, 0x30];
        let new: &[u8] = &[0xFF, 0xFE, 0x31];
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn strip_dates_time_only_not_matched() {
        // 日付部分のない時刻のみのパターンは DATE_RE にマッチしない
        let s = "meeting at 12:30:00 today";
        assert_eq!(strip_dates(s), s);
    }

    // --- テストカバレッジ補完: テーブル圧縮の空白バリエーション ---

    #[test]
    fn compact_table_row_tab_padding() {
        // タブ文字によるセルパディングも圧縮される
        assert_eq!(compact_markdown("|\ta\t|\tb\t|"), "| a | b |");
    }

    // --- テストカバレッジ補完: ブロッククォート内リンク ---

    #[test]
    fn resolve_link_in_blockquote() {
        // ブロッククォート内のリンクも正しく URL 解決される
        assert_eq!(
            resolve_markdown_urls("> [link](./page)", BASE),
            "> [link](https://example.com/docs/en/page)",
        );
    }

    #[test]
    fn resolve_nested_blockquote_link() {
        // 多段ブロッククォート内のリンクも解決される
        assert_eq!(
            resolve_markdown_urls(">> [deep](./nested)", BASE),
            ">> [deep](https://example.com/docs/en/nested)",
        );
    }

    #[test]
    fn resolve_link_inside_blockquote_fence_unchanged() {
        // ブロッククォート内のフェンスコードはコードとして扱い、URL 解決しない
        let input = "> ```\n> [skip](./code)\n> ```\n> [real](./page)";
        let expected = "> ```\n> [skip](./code)\n> ```\n> [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_nested_blockquote_fence_unchanged() {
        // 多段ブロッククォートのフェンスコード内も URL 解決対象から除外する
        let input = ">> ```\n>> [skip](./code)\n>> ```\n>> [real](./page)";
        let expected =
            ">> ```\n>> [skip](./code)\n>> ```\n>> [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- テストカバレッジ補完: 標準・山括弧混在リンク ---

    #[test]
    fn resolve_mixed_standard_and_angle_bracket_links() {
        // 同一行内で標準形式と山括弧形式のリンクが混在する場合
        let input = "[a](./x) [b](<./y z>)";
        let result = resolve_markdown_urls(input, BASE);
        assert!(result.contains("https://example.com/docs/en/x"));
        assert!(result.contains("https://example.com/docs/en/y%20z"));
    }

    // --- split_link_destination: タブ区切りの標準形式 ---

    #[test]
    fn split_link_destination_standard_with_tab_separator() {
        // タブも `is_ascii_whitespace` としてタイトルの区切りに用いられる
        let (url, title, angle) = split_link_destination("./page\t\"Title\"");
        assert!(!angle);
        assert_eq!(url, "./page");
        assert_eq!(title, "\t\"Title\"");
    }

    // --- split_link_destination: 末尾が単独バックスラッシュ ---

    #[test]
    fn split_link_destination_standard_trailing_single_backslash() {
        // 末尾のバックスラッシュ単独ではエスケープ対象がないため URL に含める
        let (url, title, angle) = split_link_destination(r"./page\");
        assert!(!angle);
        assert_eq!(url, r"./page\");
        assert_eq!(title, "");
    }

    // --- find_next_link_candidate: 開始位置が改行直後 ---

    #[test]
    fn link_candidate_start_right_after_newline_fence() {
        // 改行直後の ``` をフェンス開始と正しく認識する
        let md = "prefix\n```\n[skip](x)\n```\n[real](y)";
        let start = md.find('\n').unwrap() + 1;
        let pos = find_next_link_candidate(md, start);
        assert!(pos.is_some());
        let after = &md[pos.unwrap()..];
        assert!(after.starts_with("](y)"));
    }

    // --- Progress: 既存スピナー置き換え ---

    #[test]
    fn progress_spinner_replaces_existing_spinner() {
        // 既に表示中のスピナーがあっても、次の spinner 呼び出しで置き換えられる
        let mut p = crate::progress::Progress::new(true);
        p.spinner("first");
        // 明示的な finish を挟まず上書き
        p.spinner("second");
        p.finish_and_clear();
    }

    // --- resolve_markdown_urls: リンクテキスト内の括弧 ---

    #[test]
    fn resolve_link_text_containing_parentheses() {
        // リンクテキストに括弧が含まれても、直後のリンク先 `(url)` が先に閉じられる
        let input = "[foo (bar)](./page)";
        let result = resolve_markdown_urls(input, BASE);
        assert_eq!(result, "[foo (bar)](https://example.com/docs/en/page)");
    }

    // --- compact_markdown: フェンス直後に改行のみ（空行）があるケース ---

    #[test]
    fn compact_table_after_fence_with_blank_line() {
        // 閉じフェンスと次のテーブル行の間に空行がある場合も圧縮対象
        let input = "```\ncode\n```\n\n|  a  |  b  |";
        let expected = "```\ncode\n```\n\n| a | b |";
        assert_eq!(compact_markdown(input), expected);
    }

    // --- resolve_markdown_urls: 壊れたリンク候補が後続を止めない回帰テスト ---

    #[test]
    fn resolve_broken_link_does_not_skip_later_links() {
        // 閉じ `)` が見つからない壊れたリンクが先にあっても、
        // 後続の正常なリンクは解決される
        let input = "[x](./broken [y](./page)";
        let expected = "[x](./broken [y](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_broken_link_with_newline_keeps_later_resolution() {
        // 壊れたリンク `[incomplete](./a` の後に、改行を挟んで正常なリンク
        let input = "[incomplete](./a\n[complete](./b)";
        let expected = "[incomplete](./a\n[complete](https://example.com/docs/en/b)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_multiple_broken_links_preserves_final_link() {
        // 連続する壊れたリンク候補の後でも最終的なリンクは解決される
        let input = "[a](./x [b](./y [c](./z)";
        let expected = "[a](./x [b](./y [c](https://example.com/docs/en/z)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- resolve_markdown_urls: 既存の壊れたリンクのみ入力時の挙動 ---

    #[test]
    fn resolve_unclosed_link_in_middle_keeps_all_literal() {
        // 閉じ `)` がどこにもないリンク候補は、そのまま出力される
        let input = "before [link](./path and nothing";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    // --- resolve_markdown_urls: 壊れたリンクの後続リンク種別バリエーション ---

    #[test]
    fn resolve_broken_link_followed_by_image() {
        // 壊れたリンクの直後に画像リンクが続く場合も、画像が解決される
        let input = "[a](./broken ![img](./pic.png)";
        let expected = "[a](./broken ![img](https://example.com/docs/en/pic.png)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_broken_link_followed_by_angle_bracket_link() {
        // 壊れたリンクの後に山括弧形式のリンクが続く場合も解決される
        let input = "[a](./broken [b](<./url>)";
        let expected = "[a](./broken [b](<https://example.com/docs/en/url>)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_unclosed_angle_destination_keeps_later_resolution() {
        // 閉じ `>` がない壊れた山括弧リンク先でも、後続の正常なリンクは解決される
        let input = "[a](<./broken [b](./ok)";
        let expected = "[a](<./broken [b](https://example.com/docs/en/ok)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_broken_link_followed_by_titled_link() {
        // 壊れたリンクの後にタイトル付きリンクが続く場合も解決される
        let input = r#"[a](./broken [b](./p "title")"#;
        let expected = r#"[a](./broken [b](https://example.com/docs/en/p "title")"#;
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- fence_marker_after_blockquote の直接テスト ---

    #[test]
    fn fence_marker_after_blockquote_no_blockquote() {
        // ブロッククォート記号がない通常のフェンス行
        assert_eq!(fence_marker_after_blockquote("```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_single_level() {
        // 単一の `>` の後にフェンス
        assert_eq!(fence_marker_after_blockquote("> ```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_multiple_levels() {
        // 多段ブロッククォートの後にフェンス
        assert_eq!(fence_marker_after_blockquote(">>> ```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_mixed_spacing() {
        // ブロッククォート間に複数の空白がある場合
        assert_eq!(fence_marker_after_blockquote(">  >   ~~~"), Some(('~', 3)),);
    }

    #[test]
    fn fence_marker_after_blockquote_no_fence() {
        // ブロッククォートのみでフェンスなし
        assert_eq!(fence_marker_after_blockquote("> text"), None);
    }

    #[test]
    fn fence_marker_after_blockquote_indented() {
        // ブロッククォート前にインデントがある場合も認識される
        assert_eq!(fence_marker_after_blockquote("  > ```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_long_backtick() {
        // 4 個以上のバッククォートも検出される
        assert_eq!(fence_marker_after_blockquote("> ````rust"), Some(('`', 4)),);
    }

    #[test]
    fn fence_marker_after_blockquote_no_space_between_gt_and_fence() {
        // `>` の直後にスペースなしでフェンスマーカーが来る場合
        assert_eq!(fence_marker_after_blockquote(">```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_empty_line() {
        // 空文字列はフェンスではない
        assert_eq!(fence_marker_after_blockquote(""), None);
    }

    // --- unescape_markdown_destination の直接テスト ---

    #[test]
    fn unescape_destination_no_escapes() {
        // エスケープを含まない通常のURL
        assert_eq!(unescape_markdown_destination("./page.md"), "./page.md",);
    }

    #[test]
    fn unescape_destination_escaped_space() {
        // バックスラッシュ + 空白を実空白へ戻す
        assert_eq!(
            unescape_markdown_destination(r"./my\ file.md"),
            "./my file.md",
        );
    }

    #[test]
    fn unescape_destination_escaped_parens() {
        // バックスラッシュ + 括弧を実括弧へ戻す
        assert_eq!(
            unescape_markdown_destination(r"./file\(draft\).md"),
            "./file(draft).md",
        );
    }

    #[test]
    fn unescape_destination_escaped_gt() {
        // バックスラッシュ + > を実 > へ戻す
        assert_eq!(
            unescape_markdown_destination(r"./path\>file"),
            "./path>file",
        );
    }

    #[test]
    fn unescape_destination_preserves_other_backslashes() {
        // エスケープ対象でないバックスラッシュはそのまま残す
        assert_eq!(
            unescape_markdown_destination(r"./path\nfile"),
            r"./path\nfile",
        );
    }

    #[test]
    fn unescape_destination_trailing_backslash() {
        // 末尾のバックスラッシュ単独はそのまま残す（エスケープ対象がない）
        assert_eq!(unescape_markdown_destination(r"./path\"), r"./path\",);
    }

    #[test]
    fn unescape_destination_empty_string() {
        // 空文字列の入力は空文字列のまま
        assert_eq!(unescape_markdown_destination(""), "");
    }

    #[test]
    fn unescape_destination_multiple_escapes() {
        // 複数のエスケープが連続する場合
        assert_eq!(unescape_markdown_destination(r"\(\)\ \>"), "() >",);
    }

    #[test]
    fn unescape_destination_multibyte_with_escape() {
        // マルチバイト文字とエスケープの混在
        assert_eq!(
            unescape_markdown_destination(r"./日本語\ ファイル.md"),
            "./日本語 ファイル.md",
        );
    }

    // --- compact_markdown: ブロッククォート内テーブル ---

    #[test]
    fn compact_table_inside_blockquote_not_compressed() {
        // ブロッククォート内のテーブル行は `|` で始まらないため圧縮対象外
        let input = "> | a   | b   |";
        assert_eq!(compact_markdown(input), input);
    }

    // --- compact_markdown: ブロッククォート内フェンス内テーブル ---

    #[test]
    fn compact_blockquote_fence_preserves_inner_lines() {
        // ブロッククォート内のフェンスコード内のテーブル行はそのまま保持
        let input = "> ```\n> | padded   | table   |\n> ```";
        assert_eq!(compact_markdown(input), input);
    }

    // --- リンク検出のエスケープ・コード領域 回帰テスト ---

    #[test]
    fn resolve_escaped_close_bracket_is_not_link() {
        // `\]` でエスケープされた `]` をリンク終端として扱ってはならない。
        let input = r"[x\](./a)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_after_escaped_backtick() {
        // 先頭の `\`` はリテラルなので、後続のリンクは解決される。
        let input = r"\` [link](./a) `";
        let expected = r"\` [link](https://example.com/docs/en/a) `";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_open_bracket_inside_inline_code_is_not_link() {
        // インラインコード内の `[` は開き括弧としてカウントしない。
        let input = "`[`](./a)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_even_backslashes_before_close_bracket_is_link() {
        // 偶数個のバックスラッシュは打ち消し合い、`]` はエスケープされない。
        // `\\\\](url)` → `\\\\` の後の `]` は通常のリンク終端。
        let input = r"[x\\](./a)";
        let expected = r"[x\\](https://example.com/docs/en/a)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_escaped_open_bracket_does_not_open_link() {
        // `\[` でエスケープされた `[` は開き括弧としてカウントしない。
        // → `](./a)` の前に開き `[` がない扱いになる。
        let input = r"\[x](./a)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_long_backslash_run_then_link() {
        // 直前に長いバックスラッシュ列があってもエスケープ判定が O(1) で
        // 動作することを保証する（バックトラックなし）。
        let mut input = String::new();
        for _ in 0..1000 {
            input.push('\\');
        }
        // `\\` を 1000 個 (偶数) → 直後の `]` はエスケープされない。
        // 開き `[` がないのでリンクではない。
        let no_open = format!("{input}](./a)");
        assert_eq!(resolve_markdown_urls(&no_open, BASE), no_open);

        // 開き `[` があり、偶数個のバックスラッシュ後の `]` は通常のリンク終端。
        let with_open = format!("[x{input}](./a)");
        let expected_with_open = format!("[x{input}](https://example.com/docs/en/a)");
        assert_eq!(resolve_markdown_urls(&with_open, BASE), expected_with_open);
    }

    #[test]
    fn link_candidate_resume_after_backslash_is_escaped() {
        // start 位置以前のバックスラッシュ列を引き継ぐことを保証する。
        // 旧実装の `is_escaped_markdown_char(md, start)` と等価でなければならない。
        // `[x\](./a)` の `]`(=index 3) から再開した場合、直前の `\` で
        // `]` がエスケープされている扱いになり、リンク候補ではない。
        let md = r"[x\](./a)";
        let (pos, count) = super::find_next_link_candidate(md, 3, 1);
        assert_eq!(pos, None);
        assert_eq!(count, 1);
    }

    #[test]
    fn link_candidate_resume_after_even_backslashes_is_link() {
        // 偶数個のバックスラッシュ列の直後から再開した場合は、
        // バックスラッシュは打ち消し合い、`]` はエスケープされない。
        // `[x\\](./a)` の `]`(=index 4) から再開すると、直前 2 個の `\` は
        // 打ち消し合うため、`](`はリンク候補となる。
        let md = r"[x\\](./a)";
        let (pos, count) = super::find_next_link_candidate(md, 4, 1);
        assert_eq!(pos, Some(4));
        assert_eq!(count, 1);
    }

    #[test]
    fn link_candidate_resume_treats_escaped_open_bracket_as_literal() {
        // 再開位置直前の `\` で `[` がエスケープされる場合、
        // 開き括弧としてカウントしないことを確認する。
        // `\[x](./a)` の `[`(=index 1) から再開すると、直前の `\` で
        // エスケープされ、開き括弧は増えない。`](` の前に開き `[` がない
        // ためリンク候補にならない。
        let md = r"\[x](./a)";
        let (pos, count) = super::find_next_link_candidate(md, 1, 0);
        assert_eq!(pos, None);
        assert_eq!(count, 0);
    }

    #[test]
    fn link_candidate_resume_treats_escaped_backtick_as_literal() {
        // 再開位置直前の `\` で `` ` `` がエスケープされる場合、
        // インラインコード開始としてカウントしないことを確認する。
        // `\`` の `` ` ``(=index 1) から再開すると、直前の `\` でエスケープされ、
        // インラインコード扱いにならず、後続の `[link](./a)` は通常通り検出される。
        let md = r"\`[link](./a)";
        let (pos, count) = super::find_next_link_candidate(md, 1, 0);
        // `](` の位置は index 7。
        // `[` が index 2、link が index 3..6、`]` が index 7 にある。
        // 実際には `\` (1byte) + `` ` `` (1byte) + `[` (1byte) + `link` (4byte) = index 7 が `]`
        assert_eq!(pos, Some(7));
        assert!(count >= 1);
    }

    #[test]
    fn link_candidate_resume_after_multibyte_char_is_safe() {
        // 再開位置直前がマルチバイト文字の場合でも、
        // バイト列の継続バイト (0x80-0xBF) は `\` (0x5C) と衝突せず安全。
        // `あ[x](./a)` で `[` の手前 (= 3, あの直後) から再開する。
        let md = "あ[x](./a)";
        // "あ" は UTF-8 で 3 バイト
        let start = "あ".len();
        let (pos, count) = super::find_next_link_candidate(md, start, 0);
        // `]` の位置は あ(3) + [(1) + x(1) = 5
        assert_eq!(pos, Some(5));
        assert_eq!(count, 1);
    }

    // --- is_closing_fence_line の直接テスト ---

    #[test]
    fn closing_fence_line_pure_marker_only() {
        // マーカーのみは閉じフェンスとして妥当
        assert!(is_closing_fence_line("```", '`', 3));
        assert!(is_closing_fence_line("~~~", '~', 3));
    }

    #[test]
    fn closing_fence_line_trailing_spaces_only_allowed() {
        // マーカー後の空白/タブのみは閉じフェンスとして妥当
        assert!(is_closing_fence_line("```   ", '`', 3));
        assert!(is_closing_fence_line("```\t\t", '`', 3));
    }

    #[test]
    fn closing_fence_line_rejects_info_string() {
        // CommonMark 仕様: 閉じフェンスは info string を含んではならない
        assert!(!is_closing_fence_line("```rust", '`', 3));
        assert!(!is_closing_fence_line("```python ", '`', 3));
        assert!(!is_closing_fence_line("~~~text", '~', 3));
    }

    #[test]
    fn closing_fence_line_rejects_shorter_marker() {
        // 開始フェンスより短いマーカーは閉じない
        assert!(!is_closing_fence_line("``", '`', 3));
        assert!(!is_closing_fence_line("```", '`', 4));
    }

    #[test]
    fn closing_fence_line_accepts_longer_marker() {
        // 開始フェンスより長いマーカーは閉じる
        assert!(is_closing_fence_line("`````", '`', 3));
        assert!(is_closing_fence_line("`````   ", '`', 3));
    }

    #[test]
    fn closing_fence_line_rejects_different_marker_char() {
        // 異なるマーカー文字では閉じない
        assert!(!is_closing_fence_line("~~~", '`', 3));
        assert!(!is_closing_fence_line("```", '~', 3));
    }

    // --- is_closing_fence_after_blockquote の直接テスト ---

    #[test]
    fn closing_fence_after_blockquote_with_marker_only() {
        // ブロッククォート記号 + マーカーのみは閉じフェンスとして妥当
        assert!(is_closing_fence_after_blockquote("> ```", '`', 3));
        assert!(is_closing_fence_after_blockquote(">> ~~~", '~', 3));
    }

    #[test]
    fn closing_fence_after_blockquote_rejects_info_string() {
        // ブロッククォート内でも info string 付きは閉じフェンスにしない
        assert!(!is_closing_fence_after_blockquote("> ```rust", '`', 3));
        assert!(!is_closing_fence_after_blockquote(">> ```py", '`', 3));
    }

    // --- compact_markdown: info string 付きの行を閉じフェンスとして誤認しない回帰テスト ---

    #[test]
    fn compact_markdown_does_not_close_fence_on_info_string_line() {
        // フェンス内の info string 付き行 (例: ```python) を閉じフェンスとして
        // 誤認すると、その後のテーブル行が圧縮対象になってしまう。
        let input = "\
```
```rust
| inside  | fence  |
```
| outside  | fence  |";
        // 修正後: ```rust は閉じフェンスではない → 最初の ``` だけがフェンスを閉じる
        // ↓ "```" がフェンスを開き、"```rust" はコード内、"| inside  | fence  |" もコード内、
        //   "```" がフェンスを閉じ、"| outside  | fence  |" が圧縮される
        let expected = "\
```
```rust
| inside  | fence  |
```
| outside | fence |";
        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_markdown_does_not_close_fence_on_info_string_then_compress_outer_table() {
        // フェンス内の info string 付き行 (```python) を閉じフェンスとして扱わず、
        // 外側のテーブル行のみ圧縮されることを確認する。
        let input = "\
```
```python
inside-of-fence
```
| outside  | table  |";
        let expected = "\
```
```python
inside-of-fence
```
| outside | table |";
        assert_eq!(compact_markdown(input), expected);
    }

    // --- resolve_markdown_urls: info string 付きの行を閉じフェンスとして誤認しない回帰テスト ---

    #[test]
    fn resolve_markdown_urls_does_not_close_fence_on_info_string_line() {
        // フェンス内の info string 付き行 (```rust) を閉じフェンスとして
        // 誤認するとフェンス内のリンクが URL 解決対象になってしまう。
        let input = "\
```
```rust
[skip](./inside)
```
[resolved](./outside)";
        let expected = "\
```
```rust
[skip](./inside)
```
[resolved](https://example.com/docs/en/outside)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_markdown_urls_does_not_close_blockquote_fence_on_info_string_line() {
        // ブロッククォート内のフェンスでも info string 付き行は閉じフェンスにしない。
        let input = "\
> ```
> ```python
> [skip](./inside)
> ```
> [resolved](./outside)";
        let expected = "\
> ```
> ```python
> [skip](./inside)
> ```
> [resolved](https://example.com/docs/en/outside)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_markdown_urls_closes_fence_on_crlf_line_endings() {
        // CRLF 改行のフェンスでも閉じフェンスが正しく成立し、後続のリンクが解決される。
        // `find_next_link_candidate` は `\n` で分割するため line 末尾に `\r` が残るが、
        // `is_closing_fence_line` は末尾 `\r` を line ending として無視する。
        let input = "```\r\ncode\r\n```\r\n[link](./page)";
        let expected = "```\r\ncode\r\n```\r\n[link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- is_closing_fence_line: CRLF 改行末尾の許容 ---

    #[test]
    fn closing_fence_line_trailing_cr_allowed() {
        // 末尾 CR (`\r`) は line ending の一部として無視され、閉じフェンスとして妥当。
        assert!(is_closing_fence_line("```\r", '`', 3));
        assert!(is_closing_fence_line("```   \r", '`', 3));
    }

    #[test]
    fn closing_fence_line_rejects_info_string_with_cr() {
        // 末尾 CR を取り除いた上でも info string 付きは閉じ扱いしない
        assert!(!is_closing_fence_line("```rust\r", '`', 3));
    }

    // --- url_has_balanced_parens の直接テスト ---

    #[test]
    fn url_balanced_parens_simple_balanced() {
        assert!(url_has_balanced_parens(
            "https://example.com/wiki/Rust_(language)"
        ));
    }

    #[test]
    fn url_balanced_parens_no_parens() {
        assert!(url_has_balanced_parens("https://example.com/page.html"));
    }

    #[test]
    fn url_balanced_parens_empty() {
        assert!(url_has_balanced_parens(""));
    }

    #[test]
    fn url_balanced_parens_unbalanced_open() {
        // 開きパーレンが多すぎる
        assert!(!url_has_balanced_parens("https://example.com/(draft"));
    }

    #[test]
    fn url_balanced_parens_unbalanced_close_first() {
        // 閉じパーレンが先に来る場合は即座に false
        assert!(!url_has_balanced_parens("https://example.com/draft)"));
    }

    #[test]
    fn url_balanced_parens_nested_balanced() {
        // ネストした括弧もバランスしていれば true
        assert!(url_has_balanced_parens("a(b(c)d)e"));
    }

    #[test]
    fn url_balanced_parens_nested_close_imbalance() {
        // ネストの途中で閉じ過剰
        assert!(!url_has_balanced_parens("a(b)c)d"));
    }

    // --- write_resolved_url の直接テスト ---

    #[test]
    fn write_resolved_url_standard_balanced() {
        // 標準形式 + バランスした URL は山括弧なし
        let mut out = String::new();
        write_resolved_url(&mut out, "https://example.com/page", false);
        assert_eq!(out, "https://example.com/page");
    }

    #[test]
    fn write_resolved_url_standard_unbalanced_forces_angle() {
        // アンバランスな URL は山括弧で出力
        let mut out = String::new();
        write_resolved_url(&mut out, "https://example.com/(draft", false);
        assert_eq!(out, "<https://example.com/(draft>");
    }

    #[test]
    fn write_resolved_url_angle_always_wraps() {
        // use_angle_brackets=true は常に山括弧
        let mut out = String::new();
        write_resolved_url(&mut out, "https://example.com/page", true);
        assert_eq!(out, "<https://example.com/page>");
    }

    #[test]
    fn write_resolved_url_angle_with_balanced_parens() {
        // 山括弧指定 + バランスした URL でも山括弧で囲む
        let mut out = String::new();
        write_resolved_url(&mut out, "https://example.com/(a)", true);
        assert_eq!(out, "<https://example.com/(a)>");
    }

    // --- strip_blockquote_markers の直接テスト ---

    #[test]
    fn strip_blockquote_markers_no_marker() {
        // ブロッククォート記号がない通常の行
        assert_eq!(strip_blockquote_markers("plain text"), Some("plain text"));
    }

    #[test]
    fn strip_blockquote_markers_single_level_with_space() {
        // 単一の `>` + スペースを取り除く
        assert_eq!(strip_blockquote_markers("> content"), Some("content"));
    }

    #[test]
    fn strip_blockquote_markers_single_level_no_space() {
        // `>` の直後にスペースがない場合も取り除く
        assert_eq!(strip_blockquote_markers(">content"), Some("content"));
    }

    #[test]
    fn strip_blockquote_markers_multiple_levels() {
        // 多段ブロッククォート記号を順に取り除く
        assert_eq!(strip_blockquote_markers(">>> deep"), Some("deep"));
    }

    #[test]
    fn strip_blockquote_markers_with_leading_indent() {
        // 行頭インデントも取り除いたうえでブロッククォート記号を処理
        assert_eq!(strip_blockquote_markers("   > content"), Some("content"));
    }

    #[test]
    fn strip_blockquote_markers_mixed_spacing_between_markers() {
        // ブロッククォート間に複数空白がある場合でも順に剥がせる
        assert_eq!(strip_blockquote_markers(">  >   inner"), Some("inner"));
    }

    #[test]
    fn strip_blockquote_markers_empty_line() {
        // 空行はそのまま空文字列を返す
        assert_eq!(strip_blockquote_markers(""), Some(""));
    }

    #[test]
    fn strip_blockquote_markers_only_markers() {
        // ブロッククォート記号だけで内容がない行
        assert_eq!(strip_blockquote_markers(">>"), Some(""));
    }

    // --- is_closing_fence_after_blockquote の追加境界テスト ---

    #[test]
    fn closing_fence_after_blockquote_trailing_spaces_allowed() {
        // ブロッククォート内の閉じフェンス + 末尾空白
        assert!(is_closing_fence_after_blockquote("> ```   ", '`', 3));
    }

    #[test]
    fn closing_fence_after_blockquote_trailing_cr_allowed() {
        // ブロッククォート内でも末尾 CR は line ending として無視
        assert!(is_closing_fence_after_blockquote("> ```\r", '`', 3));
    }

    #[test]
    fn closing_fence_after_blockquote_longer_marker() {
        // 開始フェンスより長い閉じマーカーも有効
        assert!(is_closing_fence_after_blockquote(">> `````", '`', 3));
    }

    #[test]
    fn closing_fence_after_blockquote_shorter_marker_rejected() {
        // 開始フェンスより短いマーカーは閉じ扱いしない
        assert!(!is_closing_fence_after_blockquote("> ```", '`', 5));
    }

    #[test]
    fn closing_fence_after_blockquote_different_marker_rejected() {
        // 異なるマーカー文字は閉じ扱いしない
        assert!(!is_closing_fence_after_blockquote("> ~~~", '`', 3));
    }

    // --- resolve_markdown_urls: ブロッククォート内 CRLF フェンスの境界 ---

    #[test]
    fn resolve_markdown_urls_closes_blockquote_fence_on_crlf() {
        // ブロッククォート内のフェンスが CRLF でも正しく閉じ、後続のリンクが解決される
        let input = "> ```\r\n> code\r\n> ```\r\n> [link](./page)";
        let expected = "> ```\r\n> code\r\n> ```\r\n> [link](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- unescape_markdown_destination: バックスラッシュとエスケープ対象の混在 ---

    #[test]
    fn unescape_destination_backslash_then_unrelated_char() {
        // バックスラッシュの直後がエスケープ対象でない場合、両方そのまま残す
        assert_eq!(unescape_markdown_destination(r"\a\b"), r"\a\b");
    }

    #[test]
    fn unescape_destination_consecutive_backslashes_before_escape_target() {
        // `\\(` は最初の `\` がリテラル、2 つ目以降の `\(` が実括弧に展開される
        assert_eq!(unescape_markdown_destination(r"\\("), r"\(");
    }

    // --- 回帰テスト: エスケープされた `<` は角括弧リンク先の開始ではない ---

    #[test]
    fn find_close_paren_escaped_lt_is_not_angle_destination_start() {
        // `\<` は CommonMark のエスケープ済み `<` なので、山括弧形式リンク先の
        // 開始として扱ってはならない。
        // 入力: `\<a)` → depth=1 のまま `)` で閉じ → Some(3)
        let input = r"\<a)";
        assert_eq!(find_link_close_paren(input), Some(3));
    }

    #[test]
    fn resolve_link_with_escaped_lt_in_destination() {
        // `[x](\<a)` は標準形式リンク先 `\<a` として処理し、URL 解決後は
        // `<` が percent-encoded で出力される。
        let result = resolve_markdown_urls(r"[x](\<a)", BASE);
        assert_eq!(result, "[x](https://example.com/docs/en/%3Ca)");
    }

    #[test]
    fn unescape_destination_escaped_lt() {
        // バックスラッシュ + `<` を実 `<` へ戻す
        assert_eq!(
            unescape_markdown_destination(r"./path\<file"),
            "./path<file",
        );
    }

    #[test]
    fn resolve_link_with_escaped_lt_and_title() {
        // `\<` を含む標準形式リンク先 + title
        let result = resolve_markdown_urls(r#"[x](\<a "title")"#, BASE);
        assert_eq!(result, r#"[x](https://example.com/docs/en/%3Ca "title")"#,);
    }

    #[test]
    fn resolve_angle_bracket_url_with_escaped_lt_inside() {
        // 山括弧形式リンク先の中に `\<` が含まれる場合、URL 解決後も `<` が
        // percent-encoded で出力される。
        let result = resolve_markdown_urls(r"[x](<a\<b>)", BASE);
        assert_eq!(result, "[x](<https://example.com/docs/en/a%3Cb>)");
    }

    #[test]
    fn find_close_paren_double_backslash_lt_is_standard_destination() {
        // `\\<a)` のように `\` で始まるリンク先は、CommonMark 上「`<` で
        // 始まらない」ため標準形式として扱う。途中の `<` は山括弧形式の
        // 開始ではなく文字 `<` として処理し、`)` でリンクが閉じる。
        let input = r"\\<a)";
        assert_eq!(find_link_close_paren(input), Some(4));
    }

    #[test]
    fn resolve_link_with_leading_backslash_then_lt_in_destination() {
        // 先頭が `\` で始まるリンク先 `\\<a` は標準形式として処理される。
        // unescape_markdown_destination は `\<` を `<` に戻すため、結果として
        // `\<a` が `Url::join` に渡る。`Url::join` は path での `\` を `/`
        // と等価に解釈するため、結果はオリジン直下に解決される。
        let result = resolve_markdown_urls(r"[x](\\<a)", BASE);
        assert_eq!(result, "[x](https://example.com/%3Ca)");
    }
}
