pub mod embedding;
mod front_matter;
mod invisible;
mod markdown;
mod output;
mod progress;
#[cfg(test)]
mod test_support;

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use clap::builder::ArgPredicate;
use headless_chrome::protocol::cdp::{Network, Page, Runtime};
use headless_chrome::{Browser, LaunchOptions, Tab};
use serde::Deserialize;
use time::OffsetDateTime;
use url::Url;

use crate::front_matter::{
    MetaEntry, SourceMetadata, distinct_final_url, format_retrieved_at, front_matter_text,
    normalize_title, parse_meta_entries,
};
use crate::invisible::InvisibleCount;
use crate::markdown::{compact_markdown, resolve_markdown_urls};
use crate::output::{
    OutputWriteState, atomic_write, build_output_text, capture_output_write_state, file_status,
    keeps_existing_output, kept_file_status, write_all_and_flush,
};
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

    /// 出力の先頭に、取得元の情報を YAML の front matter として付ける。
    /// 項目は url・final_url・title・selectors・retrieved_at（取得した時刻、UTC）と --meta の項目。
    /// 既存のファイルとは retrieved_at を除いて比べ、取得した時刻だけの違いでは書き換えない。
    /// 既存のファイルの front matter にしかない項目は引き継がないので、残したい値は毎回 --meta で渡す。
    #[arg(long, default_value_if("meta", ArgPredicate::IsPresent, "true"))]
    front_matter: bool,

    /// front matter に足す項目（複数指定可）。指定すると --front-matter も有効になる。
    /// 最初の = で分け、値は空でもよい。キーは英字か _ で始め、英数字・_・- だけで書く。
    #[arg(long, value_name = "KEY=VALUE")]
    meta: Vec<String>,

    /// ページの上で見えなくされた要素とテキストも取り出す（従来の抽出）。
    /// 既定では display: none・opacity: 0・visibility: hidden・閉じた details の中身・
    /// 画面の外へ追い出した要素などを、取得した時点の表示を基準に除く。
    #[arg(long)]
    keep_hidden: bool,

    /// 見えない文字を出力から除かない（従来の出力）。
    /// 既定ではゼロ幅スペース・BOM・双方向の制御文字・タグ文字・制御文字などを除く。
    #[arg(long)]
    keep_invisible: bool,
}

type MainResponseStatus = Arc<Mutex<Option<u32>>>;

struct BrowserPage {
    browser: Browser,
    tab: Arc<Tab>,
    main_response_status: MainResponseStatus,
}

pub fn run_cli() -> Result<()> {
    let cli = Cli::parse();
    // --meta の誤りは、ブラウザを起動する前に知らせる
    let meta = parse_meta_entries(&cli.meta)?;
    let mut progress = Progress::new(!cli.quiet);
    let selectors = prepare_selectors(&cli.selector);

    let BrowserPage {
        browser: _browser,
        tab,
        main_response_status,
    } = launch_browser(&cli, &mut progress)?;
    load_page(&tab, &cli.url, cli.wait, &mut progress)?;
    validate_http_status(&main_response_status, &cli.url)?;

    let extracted = extract_html_fragments(&tab, &selectors, cli.keep_hidden, &mut progress)?;
    let strip_invisible = !cli.keep_invisible;
    let mut invisible = InvisibleCount::default();
    // 取得した時刻は、本文の HTML を取り出した直後に記録する
    let front_matter = if cli.front_matter {
        let metadata = read_source_metadata(&tab, &cli.url, &selectors, meta)?;
        Some(front_matter_text(metadata, strip_invisible, &mut invisible))
    } else {
        None
    };
    let markdown = convert_html_fragments(
        &tab,
        &extracted.fragments,
        &cli.url,
        strip_invisible,
        &mut invisible,
        &mut progress,
    )?;
    let output_text = build_output_text(markdown, front_matter.as_deref(), cli.output.is_some());
    let output_state = capture_output_write_state(cli.output.as_deref());
    write_or_print_output(&cli, &output_text, output_state, &progress)?;
    report_removals(
        &progress,
        &RemovalReport {
            hidden: extracted.hidden,
            invisible,
        },
    );

    Ok(())
}

fn prepare_selectors(selector_args: &[String]) -> Vec<String> {
    if selector_args.is_empty() {
        vec!["body".to_string()]
    } else {
        selector_args.to_vec()
    }
}

fn launch_browser(cli: &Cli, progress: &mut Progress) -> Result<BrowserPage> {
    // ブラウザを起動する
    progress.spinner("Launching Chrome...");
    let launch_options = build_launch_options(cli);

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

    Ok(BrowserPage {
        browser,
        tab,
        main_response_status,
    })
}

fn load_page(tab: &Tab, url: &str, wait_secs: u64, progress: &mut Progress) -> Result<()> {
    // ページへ遷移する
    progress.spinner(&format!("Loading page: {url}"));
    tab.navigate_to(url)
        .with_context(|| format!("Failed to navigate to URL: {url}"))?;

    tab.wait_until_navigated().context("Page load timed out")?;

    // JS 描画完了を待つための追加待機
    if wait_secs > 0 {
        progress.set_message(&format!("Waiting for JS rendering ({wait_secs}s)..."));
        std::thread::sleep(Duration::from_secs(wait_secs));
    }
    progress.finish("Page loaded");

    Ok(())
}

fn validate_http_status(main_response_status: &MainResponseStatus, url: &str) -> Result<()> {
    // HTTP ステータスコードを確認する（400 以上はエラー）。
    // ページ内 JS は改変可能なため、CDP の Network event から得た値だけを信頼する。
    let status_code = main_response_status
        .lock()
        .expect("HTTP ステータス記録用 Mutex が poisoned になった")
        .unwrap_or(0);

    if status_code >= 400 {
        bail!("HTTP {} — page not saved: {}", status_code, url);
    }

    Ok(())
}

/// セレクタ評価 JS が例外を捕捉した際に返す文字列の番兵プレフィックス。
///
/// 正規の抽出結果は JSON のオブジェクト (`{` で始まる文字列) なので、NUL で始まる
/// このプレフィックスとは衝突しない。
const SELECTOR_ERROR_SENTINEL: &str = "\u{0}get-md-selector-error\u{0}";

/// セレクタ評価結果からエラー番兵を判別し、エラーであれば例外メッセージを返す。
fn selector_evaluation_error(value: &str) -> Option<&str> {
    value.strip_prefix(SELECTOR_ERROR_SENTINEL)
}

/// セレクタに一致した要素の HTML を取り出すスクリプト (関数式)。ページの中で評価する。
///
/// `--keep-hidden` でなければ、ページの上で見えない要素とテキストを複製から削ってから
/// outerHTML にする。判定の規則はスクリプトの中に書いてある。
const EXTRACT_SCRIPT: &str = include_str!("extract.js");

/// 1 つのセレクタについて、抽出のスクリプトを呼ぶ式を組み立てる。
///
/// 無効なセレクタの例外はスクリプトの中で捕捉して番兵付きのメッセージにし、
/// 「一致 0 件」やスクリプトの誤りと区別して、分かりやすいエラーにする。
fn extraction_script(selector: &str, keep_hidden: bool) -> String {
    format!(
        "(\n{EXTRACT_SCRIPT}\n)({selector}, {keep_hidden}, {sentinel})",
        selector = escape_js_string(selector),
        sentinel = escape_js_string(SELECTOR_ERROR_SENTINEL),
    )
}

/// 抽出のスクリプトが返す、1 つのセレクタの結果 (JSON)。
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SelectorExtraction {
    /// 見えるかの判定の途中で起きた例外のメッセージ
    error: Option<String>,
    /// セレクタに一致した要素の数
    matched: u64,
    /// 出力する要素の HTML (一致した順。見えない根の要素は含まない)
    fragments: Vec<String>,
    hidden_elements: u64,
    hidden_texts: u64,
    faded_chars: u64,
    kept_chars: u64,
}

/// ページの上で見えないために除いたものの数 (抽出した断片ごとの延べ数)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HiddenRemovals {
    /// 子孫ごと除いた要素 (見えない根の要素を含む) のうち、文字か画像を含んでいたものの数
    elements: u64,
    /// テキストの単位で除いた箇所の数 (続けて除いたテキストは 1 か所と数える)
    texts: u64,
    /// opacity: 0 と visibility: hidden のために除いた文字の数 (空白を除く)
    faded_chars: u64,
    /// 残した文字の数 (空白を除く)
    kept_chars: u64,
}

impl HiddenRemovals {
    fn add(&mut self, extraction: &SelectorExtraction) {
        self.elements += extraction.hidden_elements;
        self.texts += extraction.hidden_texts;
        self.faded_chars += extraction.faded_chars;
        self.kept_chars += extraction.kept_chars;
    }
}

/// 取り出した HTML (セレクタごとに 1 つ) と、見えないために除いたものの数。
struct ExtractedHtml {
    fragments: Vec<String>,
    hidden: HiddenRemovals,
}

/// 抽出のスクリプトを動かす、ページのスクリプトから切り離した実行環境 (isolated world) を作る。
///
/// ページと同じ実行環境で動かすと、ページのスクリプトが `getComputedStyle` や `JSON.stringify`、
/// DOM のプロトタイプを書き換えて、見えるかの判定や抽出の結果そのものを偽れる (見えない指示を
/// 紛れ込ませたいページなら、そうする)。isolated world は DOM を共有しつつ、組み込みのオブジェクトと
/// DOM のプロトタイプを別に持つ (拡張機能の content script と同じ仕組み) ので、書き換えが届かない。
fn create_isolated_world(tab: &Tab) -> Result<Runtime::ExecutionContextId> {
    let frame_id = tab
        .call_method(Page::GetFrameTree(None))
        .context("Failed to get main frame")?
        .frame_tree
        .frame
        .id;
    let world = tab
        .call_method(Page::CreateIsolatedWorld {
            frame_id,
            world_name: Some("get-md".to_string()),
            grant_univeral_access: None,
        })
        .context("Failed to create an isolated world for extraction")?;
    Ok(world.execution_context_id)
}

/// 式を指定した実行環境で評価する。例外が外へ出たらエラーにする。
fn evaluate_in_context(
    tab: &Tab,
    context_id: Runtime::ExecutionContextId,
    expression: &str,
) -> Result<Runtime::RemoteObject> {
    let evaluated = tab.call_method(Runtime::Evaluate {
        expression: expression.to_string(),
        return_by_value: Some(true),
        generate_preview: Some(false),
        silent: Some(false),
        await_promise: Some(false),
        include_command_line_api: Some(false),
        user_gesture: Some(false),
        object_group: None,
        context_id: Some(context_id),
        throw_on_side_effect: None,
        timeout: None,
        disable_breaks: None,
        repl_mode: None,
        allow_unsafe_eval_blocked_by_csp: None,
        unique_context_id: None,
        serialization_options: None,
    })?;
    if let Some(details) = evaluated.exception_details {
        bail!("{}", details.text);
    }
    Ok(evaluated.result)
}

/// 抽出のスクリプトの戻り値を読む。無効なセレクタと判定の失敗はエラーにする。
fn parse_selector_extraction(selector: &str, value: &str) -> Result<SelectorExtraction> {
    if let Some(message) = selector_evaluation_error(value) {
        bail!("Invalid CSS selector '{}': {}", selector, message);
    }
    let extraction: SelectorExtraction = serde_json::from_str(value)
        .with_context(|| format!("Failed to read the elements of selector '{selector}'"))?;
    if let Some(error) = &extraction.error {
        bail!(
            "セレクタ '{selector}' の要素が見えるかを判定できませんでした: {error}。判定を省くには --keep-hidden を付けてください"
        );
    }
    Ok(extraction)
}

fn extract_html_fragments(
    tab: &Tab,
    selectors: &[String],
    keep_hidden: bool,
    progress: &mut Progress,
) -> Result<ExtractedHtml> {
    // セレクタに一致した要素の HTML を抽出する
    progress.spinner("Extracting HTML elements...");
    let context_id = create_isolated_world(tab)?;
    let mut html_fragments = Vec::new();
    let mut hidden = HiddenRemovals::default();
    let mut matched_only_hidden = false;
    for selector in selectors {
        progress.set_message(&format!("Extracting selector '{}'...", selector));

        let result =
            evaluate_in_context(tab, context_id, &extraction_script(selector, keep_hidden))
                .with_context(|| format!("Failed to evaluate selector '{}'", selector))?;
        let value = result.value.as_ref().and_then(|v| v.as_str()).unwrap_or("");
        let extraction = parse_selector_extraction(selector, value)?;
        hidden.add(&extraction);

        if extraction.matched == 0 {
            eprintln!("Warning: no elements matched selector '{}'", selector);
        } else if extraction.fragments.is_empty() {
            matched_only_hidden = true;
            eprintln!(
                "警告: セレクタ '{selector}' に一致した要素はどれもページの上で見えないため、出力しません (含めるには --keep-hidden)"
            );
        } else {
            // 1 つのセレクタに一致した要素は改行でつなぎ、1 つの断片として Markdown にする
            html_fragments.push(extraction.fragments.join("\n"));
        }
    }
    progress.finish_and_clear();

    if html_fragments.is_empty() {
        if matched_only_hidden {
            bail!(
                "セレクタに一致した要素はどれもページの上で見えないため、出力するものがありません。見えない要素も取り出すには --keep-hidden を付けてください"
            );
        }
        bail!("No elements matched the specified selectors");
    }

    Ok(ExtractedHtml {
        fragments: html_fragments,
        hidden,
    })
}

fn convert_html_fragments(
    tab: &Tab,
    html_fragments: &[String],
    fallback_base_url: &str,
    strip_invisible: bool,
    invisible: &mut InvisibleCount,
    progress: &mut Progress,
) -> Result<String> {
    // HTML を Markdown に変換する
    progress.spinner("Converting to Markdown...");

    let (markdown, removed) = convert_html_fragments_to_markdown(html_fragments, strip_invisible)?;
    *invisible += removed;
    // 見えない文字は URL を解決する前に除いてある (リンク先の見えない文字を Url::join に渡さない)
    let base_url = document_base_url(tab).unwrap_or_else(|| fallback_base_url.to_string());
    let markdown = resolve_markdown_urls(&markdown, &base_url);
    progress.finish("Converted to Markdown");

    Ok(markdown)
}

/// htmd が捨てる要素。中の文字は Markdown に出ないので、見えない文字を除くときも見ない。
const HTMD_SKIP_TAGS: [&str; 4] = ["script", "style", "noscript", "svg"];

/// HTML の断片を Markdown にし、`strip_invisible` なら見えない文字を除く。除いた数も返す。
///
/// 見えない文字は 2 段で除く。先に htmd の DOM のテキストと Markdown に出る属性から除き
/// (htmd は Markdown の記号をテキストノードの先頭の文字だけで escape するかを決めるので、
/// 後から除くと先頭のゼロ幅スペースの後ろの `# ` や `~~~` が見出しやコードフェンスに化ける)、
/// 最後に Markdown の全体にも同じ処理を当てて保証にする。各段では実際に除いた文字だけを
/// 数えるので、同じ文字を二重に数えない。
fn convert_html_fragments_to_markdown(
    html_fragments: &[String],
    strip_invisible: bool,
) -> Result<(String, InvisibleCount)> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(HTMD_SKIP_TAGS.to_vec())
        .options(htmd::options::Options {
            ul_bullet_spacing: 1,
            ol_number_spacing: 1,
            ..Default::default()
        })
        .build();
    let mut invisible = InvisibleCount::default();
    let mut md_parts = Vec::new();
    for html in html_fragments {
        let tree = converter
            .html_to_tree(html)
            .context("Failed to convert HTML to Markdown")?;
        if strip_invisible {
            invisible::remove_invisible_from_tree(&tree, &HTMD_SKIP_TAGS, &mut invisible);
        }
        md_parts.push(converter.tree_to_markdown(&tree));
    }

    let markdown = compact_markdown(&md_parts.join("\n\n---\n\n"));
    if !strip_invisible {
        return Ok((markdown, invisible));
    }
    let cleaned = match invisible::remove_invisible(&markdown, &mut invisible) {
        Cow::Owned(cleaned) => Some(cleaned),
        Cow::Borrowed(_) => None,
    };
    Ok((cleaned.unwrap_or(markdown), invisible))
}

/// 見えないために除いたものの数。完了時に標準エラーへ出す (front matter には書かない)。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RemovalReport {
    hidden: HiddenRemovals,
    invisible: InvisibleCount,
}

/// 除いたものの数を 1 行にする。何も除いていなければ None。
///
/// 例: `隠れた要素 3 個・隠れたテキスト 2 か所・見えない文字 12 個 (うち C1 制御文字 4 個) を除いた`
fn removal_summary(report: &RemovalReport) -> Option<String> {
    let mut parts = Vec::new();
    if report.hidden.elements > 0 {
        parts.push(format!("隠れた要素 {} 個", report.hidden.elements));
    }
    if report.hidden.texts > 0 {
        parts.push(format!("隠れたテキスト {} か所", report.hidden.texts));
    }
    if report.invisible.removed > 0 {
        let mut part = format!("見えない文字 {} 個", report.invisible.removed);
        if report.invisible.c1_controls > 0 {
            let _ = write!(
                part,
                " (うち C1 制御文字 {} 個)",
                report.invisible.c1_controls
            );
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join("・");
    // 括弧で終わるときだけ、閉じ括弧と「を」の間に空白を置く
    let separator = if joined.ends_with(')') { " " } else { "" };
    Some(format!("{joined}{separator}を除いた"))
}

/// opacity: 0 と visibility: hidden のために除いた文字が、残した文字と同じかそれより多いときの案内。
///
/// スクロールで表示するアニメーションを使うページでは、表示の前の本文が opacity: 0 や
/// visibility: hidden のままで、取得した時点では見えない。この 2 つで除いた量が本文より多い
/// ときだけ `--keep-hidden` を案内する (display: none で閉じたメニューなどは数えない)。
fn hidden_content_hint(hidden: &HiddenRemovals) -> Option<&'static str> {
    (hidden.faded_chars > 0 && hidden.faded_chars >= hidden.kept_chars).then_some(
        "透明 (opacity: 0) か visibility: hidden のために除いた内容が、残した本文と同じかそれより多くありました。スクロールで表示される内容なら --keep-hidden で取り出せます",
    )
}

/// 除いたものの数を、進捗の表示と同じく標準エラーへ出す (`--quiet` では出さない)。
fn report_removals(progress: &Progress, report: &RemovalReport) {
    if let Some(summary) = removal_summary(report) {
        progress.complete("🧹", &summary);
    }
    if let Some(hint) = hidden_content_hint(&report.hidden) {
        progress.complete("💡", hint);
    }
}

fn write_or_print_output(
    cli: &Cli,
    output_text: &str,
    output_state: OutputWriteState,
    progress: &Progress,
) -> Result<()> {
    // 既存のファイルと実質的に同じなら書き込みをスキップする
    // (--ignore-date の日時だけの差分と、--front-matter の retrieved_at だけの差分)
    let keep_existing = cli.output.is_some()
        && output_state.old_content.as_ref().is_some_and(|old| {
            keeps_existing_output(old, output_text, cli.front_matter, cli.ignore_date)
        });

    if keep_existing {
        let path = cli.output.as_ref().unwrap();
        let (icon, status) = kept_file_status(path, output_state.had_unstaged_changes_before);
        progress.complete(
            icon,
            &format!("{} → {} ({})", cli.url, path.display(), status),
        );
    } else {
        // 出力成功後にのみ URL 付きの完了表示を行う
        match &cli.output {
            Some(path) => {
                // 既存ファイルを直接 truncate せず、同一ディレクトリの一時ファイルへ
                // 書き込んでから rename でアトミックに置き換える。これにより書き込み中に
                // I/O エラー（ディスク容量不足など）が起きても既存ファイルの内容が失われない。
                atomic_write(path, output_text.as_bytes())?;
                let (icon, status) = file_status(
                    path,
                    output_state.file_existed_before,
                    &output_state.old_content,
                    output_text.as_bytes(),
                    output_state.had_unstaged_changes_before,
                );
                progress.complete(
                    icon,
                    &format!("{} → {} ({})", cli.url, path.display(), status),
                );
            }
            None => {
                write_all_and_flush(io::stdout().lock(), output_text.as_bytes())?;
                progress.complete("✔", &cli.url);
            }
        }
    }

    Ok(())
}

fn build_launch_options(cli: &Cli) -> LaunchOptions<'static> {
    LaunchOptions {
        headless: !cli.no_headless,
        path: cli.chrome_path.clone(),
        idle_browser_timeout: idle_browser_timeout(cli.timeout, cli.wait),
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

/// front matter に書く取得元の情報を、読み込んだページから集める。
///
/// 取得した時刻はこの関数を呼んだ時点で記録する。
fn read_source_metadata(
    tab: &Tab,
    url: &str,
    selectors: &[String],
    meta: Vec<MetaEntry>,
) -> Result<SourceMetadata> {
    let retrieved_at = format_retrieved_at(OffsetDateTime::now_utc())?;
    Ok(SourceMetadata {
        url: url.to_string(),
        final_url: distinct_final_url(url, &tab.get_url()),
        title: document_title(tab),
        selectors: selectors.to_vec(),
        retrieved_at,
        meta,
    })
}

/// ページの `document.title` を読む。取れない・空なら None。
fn document_title(tab: &Tab) -> Option<String> {
    tab.evaluate("document.title", false)
        .ok()
        .and_then(|result| result.value)
        .and_then(|value| value.as_str().and_then(normalize_title))
}

/// CDP 接続のアイドルタイムアウトを求める。
///
/// headless_chrome は「CDP メッセージが 1 通も来ない時間」がこの値を超えると
/// WebSocket を閉じ、以後の `call_method` をすべて失敗させる。`--wait` の待機は
/// `thread::sleep` でブロックし、静的ページはその間 CDP イベントを一切出さないため、
/// 待機時間を算入しないと `--wait` が `--timeout + 30` を超えた指定で必ず切断される。
fn idle_browser_timeout(timeout_secs: u64, wait_secs: u64) -> Duration {
    Duration::from_secs(timeout_secs.saturating_add(wait_secs).saturating_add(30))
}

/// CSS セレクタ文字列を JavaScript 文字列リテラルとしてエスケープする。
///
/// 改行/CR/タブは可読性のため短縮エスケープ(\n/\r/\t)を使い、それ以外の
/// 制御文字(NUL を含む U+0000 から U+001F)は \uXXXX で表現する。
/// 制御文字を素通しすると CSS パーサや CDP 経由のプロトコル層で
/// 予期しない挙動を起こすため、すべて何らかのエスケープに変換する。
fn escape_js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            '\u{2028}' => out.push_str(r"\u2028"),
            '\u{2029}' => out.push_str(r"\u2029"),
            // 残りの制御文字(NUL を含む U+0000 から U+001F)は \uXXXX で表現する
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::front_matter::{render_front_matter, split_front_matter};
    use crate::output::{front_matter_output_matches, is_date_only_change};
    use crate::test_support::{make_temp_dir, meta_entry, sample_metadata};
    use clap::CommandFactory;

    fn cli_with_output(output: PathBuf, ignore_date: bool) -> Cli {
        Cli {
            url: "https://example.com".to_string(),
            selector: Vec::new(),
            output: Some(output),
            chrome_path: None,
            wait: 2,
            timeout: 60,
            no_headless: false,
            no_cache: false,
            ignore_certificate_errors: false,
            quiet: true,
            ignore_date,
            front_matter: false,
            meta: Vec::new(),
            keep_hidden: false,
            keep_invisible: false,
        }
    }

    /// front matter を付けるファイル出力の CLI 設定。
    fn cli_with_front_matter_output(output: PathBuf, ignore_date: bool) -> Cli {
        Cli {
            front_matter: true,
            ..cli_with_output(output, ignore_date)
        }
    }

    #[test]
    fn prepare_selectors_defaults_to_body() {
        assert_eq!(prepare_selectors(&[]), vec!["body".to_string()]);
    }

    #[test]
    fn prepare_selectors_preserves_multiple_selectors() {
        let selectors = vec!["main".to_string(), "article".to_string()];
        assert_eq!(prepare_selectors(&selectors), selectors);
    }

    #[test]
    fn validate_http_status_accepts_missing_and_success_status() {
        let status = Arc::new(Mutex::new(None));
        assert!(validate_http_status(&status, "https://example.com").is_ok());

        *status.lock().unwrap() = Some(200);
        assert!(validate_http_status(&status, "https://example.com").is_ok());
    }

    #[test]
    fn validate_http_status_rejects_client_error() {
        let status = Arc::new(Mutex::new(Some(404)));
        let err = validate_http_status(&status, "https://example.com/missing")
            .unwrap_err()
            .to_string();

        assert!(err.contains("HTTP 404"));
        assert!(err.contains("https://example.com/missing"));
    }

    #[test]
    fn validate_http_status_accepts_below_400_and_rejects_400_boundary() {
        // 399 はエラー閾値 (>= 400) の直下なので通過する（境界値の下側）
        let status = Arc::new(Mutex::new(Some(399)));
        assert!(validate_http_status(&status, "https://example.com").is_ok());

        // 400 はちょうど閾値でエラーになる（境界値）
        let status = Arc::new(Mutex::new(Some(400)));
        assert!(validate_http_status(&status, "https://example.com").is_err());
    }

    #[test]
    fn validate_http_status_accepts_redirects_and_rejects_server_errors() {
        // 3xx リダイレクトは成功扱い（< 400）
        for code in [301u32, 302, 304, 308] {
            let status = Arc::new(Mutex::new(Some(code)));
            assert!(
                validate_http_status(&status, "https://example.com").is_ok(),
                "ステータス {code} は通過すべき"
            );
        }

        // 5xx サーバエラーはメッセージ付きで拒否する
        for code in [500u32, 503] {
            let status = Arc::new(Mutex::new(Some(code)));
            let err = validate_http_status(&status, "https://example.com")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("HTTP {code}")),
                "ステータス {code} は拒否しメッセージに含むべき: {err}"
            );
        }
    }

    #[test]
    fn write_or_print_output_ignore_date_skips_date_only_change() {
        let dir = make_temp_dir("get-md-ignore-date-skip");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, "Updated: 2026-04-12 09:00\n").expect("failed to write fixture file");

        let cli = cli_with_output(path.clone(), true);
        let output_state = capture_output_write_state(Some(&path));
        let progress = Progress::new(false);

        write_or_print_output(&cli, "Updated: 2026-04-13 10:00\n", output_state, &progress)
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, "Updated: 2026-04-12 09:00\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_ignore_date_writes_non_date_change() {
        let dir = make_temp_dir("get-md-ignore-date-write");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, "Updated: 2026-04-12 09:00\nStatus: pending\n")
            .expect("failed to write fixture file");

        let cli = cli_with_output(path.clone(), true);
        let output_state = capture_output_write_state(Some(&path));
        let progress = Progress::new(false);

        write_or_print_output(
            &cli,
            "Updated: 2026-04-13 10:00\nStatus: done\n",
            output_state,
            &progress,
        )
        .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, "Updated: 2026-04-13 10:00\nStatus: done\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_creates_missing_file_output() {
        let dir = make_temp_dir("get-md-write-create");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");

        let cli = cli_with_output(path.clone(), false);
        let output_state = capture_output_write_state(Some(&path));
        let progress = Progress::new(false);

        write_or_print_output(&cli, "new content\n", output_state, &progress)
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, "new content\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_without_ignore_date_overwrites_date_only_change() {
        let dir = make_temp_dir("get-md-write-no-ignore-date");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, "Updated: 2026-04-12 09:00\n").expect("failed to write fixture file");

        let cli = cli_with_output(path.clone(), false);
        let output_state = capture_output_write_state(Some(&path));
        let progress = Progress::new(false);

        write_or_print_output(&cli, "Updated: 2026-04-13 10:00\n", output_state, &progress)
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, "Updated: 2026-04-13 10:00\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `sample_metadata` より後に取得した時刻。
    const LATER_RETRIEVED_AT: &str = "2026-10-01T12:34:56Z";

    /// `sample_metadata` を後で取り直したときの情報 (取得した時刻だけが違う)。
    fn refetched_metadata() -> SourceMetadata {
        SourceMetadata {
            retrieved_at: LATER_RETRIEVED_AT.to_string(),
            ..sample_metadata()
        }
    }

    /// front matter 付きのファイル出力の内容を組み立てる。
    fn front_matter_document(metadata: &SourceMetadata, body: &str) -> String {
        build_output_text(body.to_string(), Some(&render_front_matter(metadata)), true)
    }

    /// 既存のファイル (front matter 付き) を書き換えずに残すかを判定する。
    fn keeps_front_matter_file(
        old: (&SourceMetadata, &str),
        new: (&SourceMetadata, &str),
        ignore_date: bool,
    ) -> bool {
        let old = front_matter_document(old.0, old.1);
        let new = front_matter_document(new.0, new.1);
        keeps_existing_output(old.as_bytes(), &new, true, ignore_date)
    }

    #[test]
    fn cli_front_matter_flag_enables_front_matter() {
        let cli = Cli::try_parse_from(["get-md", "https://example.com", "--front-matter"]).unwrap();
        assert!(cli.front_matter);
        assert!(cli.meta.is_empty());
    }

    #[test]
    fn cli_meta_alone_enables_front_matter() {
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "--meta",
            "source=manual",
            "--meta",
            "note=",
        ])
        .unwrap();
        assert!(
            cli.front_matter,
            "--meta だけで front matter が有効になること"
        );
        assert_eq!(cli.meta, vec!["source=manual", "note="]);
    }

    #[test]
    fn cli_help_lists_front_matter_options() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--front-matter"), "{help}");
        assert!(help.contains("--meta <KEY=VALUE>"), "{help}");
    }

    #[test]
    fn render_front_matter_lists_default_body_selector() {
        let metadata = SourceMetadata {
            selectors: prepare_selectors(&[]),
            ..sample_metadata()
        };
        assert!(
            render_front_matter(&metadata).contains("\nselectors: [\"body\"]\n"),
            "{}",
            render_front_matter(&metadata)
        );
    }

    #[test]
    fn render_front_matter_ends_with_closing_line_and_blank_line() {
        let rendered = render_front_matter(&sample_metadata());
        assert!(rendered.ends_with("\n---\n\n"), "{rendered}");
        assert_eq!(
            front_matter_document(&sample_metadata(), "Body"),
            format!("{rendered}Body\n")
        );
    }

    #[test]
    fn front_matter_is_separable_when_body_starts_with_thematic_break() {
        let body = "---\n\nAfter a rule\n\n---\n\nMore\n";
        let document = front_matter_document(&sample_metadata(), body);
        let (header, rest) = split_front_matter(&document).expect("front matter is found");

        assert_eq!(
            format!("---\n{header}---\n\n"),
            render_front_matter(&sample_metadata())
        );
        // 閉じの `---` の後の空行を挟んで、本文の `---` がそのまま続く
        assert_eq!(rest, format!("\n{body}"));
    }

    #[test]
    fn build_output_text_puts_front_matter_on_stdout_output() {
        let header = render_front_matter(&sample_metadata());
        // 標準出力でも本文の前に front matter を付け、末尾の改行は従来どおり足さない
        assert_eq!(
            build_output_text("Body".to_string(), Some(&header), false),
            format!("{header}Body")
        );
    }

    #[test]
    fn build_output_text_adds_trailing_newline_for_file_output_with_front_matter() {
        let header = render_front_matter(&sample_metadata());
        assert_eq!(
            build_output_text("Body".to_string(), Some(&header), true),
            format!("{header}Body\n")
        );
        assert_eq!(
            build_output_text("Body\n".to_string(), Some(&header), true),
            format!("{header}Body\n")
        );
    }

    #[test]
    fn front_matter_comparison_ignores_retrieved_at() {
        for ignore_date in [false, true] {
            assert!(
                keeps_front_matter_file(
                    (&sample_metadata(), "Body"),
                    (&refetched_metadata(), "Body"),
                    ignore_date,
                ),
                "ignore_date={ignore_date}"
            );
        }
    }

    #[test]
    fn front_matter_comparison_detects_changed_items() {
        let changed = [
            SourceMetadata {
                title: Some("Renamed".to_string()),
                ..refetched_metadata()
            },
            SourceMetadata {
                title: None,
                ..refetched_metadata()
            },
            SourceMetadata {
                url: "https://example.com/other".to_string(),
                ..refetched_metadata()
            },
            SourceMetadata {
                final_url: None,
                ..refetched_metadata()
            },
            SourceMetadata {
                selectors: vec!["main".to_string()],
                ..refetched_metadata()
            },
            SourceMetadata {
                meta: vec![meta_entry("source", "changed"), meta_entry("note", "")],
                ..refetched_metadata()
            },
            SourceMetadata {
                meta: vec![meta_entry("source", "manual")],
                ..refetched_metadata()
            },
            SourceMetadata {
                meta: vec![
                    meta_entry("source", "manual"),
                    meta_entry("note", ""),
                    meta_entry("retrieved_at_note", "added"),
                ],
                ..refetched_metadata()
            },
        ];
        for new in &changed {
            for ignore_date in [false, true] {
                assert!(
                    !keeps_front_matter_file(
                        (&sample_metadata(), "Body"),
                        (new, "Body"),
                        ignore_date,
                    ),
                    "{new:?} ignore_date={ignore_date}"
                );
            }
        }
    }

    #[test]
    fn front_matter_comparison_detects_body_change() {
        for ignore_date in [false, true] {
            assert!(
                !keeps_front_matter_file(
                    (&sample_metadata(), "Body"),
                    (&refetched_metadata(), "Changed body"),
                    ignore_date,
                ),
                "ignore_date={ignore_date}"
            );
        }
    }

    #[test]
    fn front_matter_comparison_with_ignore_date_skips_date_only_body_change() {
        assert!(keeps_front_matter_file(
            (&sample_metadata(), "Updated: 2026-04-12 09:00"),
            (&refetched_metadata(), "Updated: 2026-04-13 10:00"),
            true,
        ));
    }

    #[test]
    fn front_matter_comparison_without_ignore_date_detects_date_only_body_change() {
        assert!(!keeps_front_matter_file(
            (&sample_metadata(), "Updated: 2026-04-12 09:00"),
            (&refetched_metadata(), "Updated: 2026-04-13 10:00"),
            false,
        ));
    }

    #[test]
    fn front_matter_comparison_with_ignore_date_compares_front_matter_strictly() {
        // --ignore-date は本文にだけ当てる。front matter の日時に見える値の変化は見逃さない
        let old = SourceMetadata {
            meta: vec![meta_entry("published", "2026-04-12")],
            ..sample_metadata()
        };
        let new = SourceMetadata {
            meta: vec![meta_entry("published", "2026-04-13")],
            ..refetched_metadata()
        };
        assert!(!keeps_front_matter_file(
            (&old, "Updated: 2026-04-12 09:00"),
            (&new, "Updated: 2026-04-13 10:00"),
            true,
        ));
    }

    #[test]
    fn front_matter_comparison_requires_retrieved_at_line_in_existing_file() {
        let new = front_matter_document(&refetched_metadata(), "Body");
        let old = front_matter_document(&sample_metadata(), "Body")
            .replace("retrieved_at: \"2026-09-29T03:10:00Z\"\n", "");
        assert!(!keeps_existing_output(old.as_bytes(), &new, true, false));
    }

    #[test]
    fn front_matter_comparison_falls_back_when_existing_file_has_no_front_matter() {
        let new = front_matter_document(&sample_metadata(), "Updated: 2026-04-13 10:00");
        // 初回 (本文だけのファイル)・閉じの行がない・UTF-8 でないファイルは、従来の比較に任せる
        let old_files: [&[u8]; 3] = [
            b"Updated: 2026-04-12 09:00\n",
            b"---\ntitle: x\n",
            &[0xff, 0xfe, 0x00],
        ];
        for old in old_files {
            for ignore_date in [false, true] {
                assert_eq!(front_matter_output_matches(old, &new, ignore_date), None);
                assert_eq!(
                    keeps_existing_output(old, &new, true, ignore_date),
                    ignore_date && is_date_only_change(old, new.as_bytes()),
                );
                assert!(!keeps_existing_output(old, &new, true, ignore_date));
            }
        }
    }

    #[test]
    fn write_or_print_output_front_matter_keeps_file_when_only_retrieved_at_differs() {
        let dir = make_temp_dir("get-md-front-matter-keep");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        let old = front_matter_document(&sample_metadata(), "Body");
        std::fs::write(&path, &old).expect("failed to write fixture file");

        let cli = cli_with_front_matter_output(path.clone(), false);
        let output_state = capture_output_write_state(Some(&path));
        let new = front_matter_document(&refetched_metadata(), "Body");
        write_or_print_output(&cli, &new, output_state, &Progress::new(false))
            .expect("failed to handle output");

        // 書き換えないので、retrieved_at は前に取得した時刻のまま
        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, old);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_front_matter_rewrites_file_when_title_changes() {
        let dir = make_temp_dir("get-md-front-matter-title");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, front_matter_document(&sample_metadata(), "Body"))
            .expect("failed to write fixture file");

        let cli = cli_with_front_matter_output(path.clone(), true);
        let output_state = capture_output_write_state(Some(&path));
        let renamed = SourceMetadata {
            title: Some("Renamed".to_string()),
            ..refetched_metadata()
        };
        let new = front_matter_document(&renamed, "Body");
        write_or_print_output(&cli, &new, output_state, &Progress::new(false))
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, new);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_front_matter_with_ignore_date_skips_date_only_body_change() {
        let dir = make_temp_dir("get-md-front-matter-ignore-date");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        let old = front_matter_document(&sample_metadata(), "Updated: 2026-04-12 09:00");
        std::fs::write(&path, &old).expect("failed to write fixture file");

        let cli = cli_with_front_matter_output(path.clone(), true);
        let output_state = capture_output_write_state(Some(&path));
        let new = front_matter_document(&refetched_metadata(), "Updated: 2026-04-13 10:00");
        write_or_print_output(&cli, &new, output_state, &Progress::new(false))
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, old);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_or_print_output_front_matter_adds_header_to_file_without_front_matter() {
        let dir = make_temp_dir("get-md-front-matter-add");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, "Body\n").expect("failed to write fixture file");

        let cli = cli_with_front_matter_output(path.clone(), false);
        let output_state = capture_output_write_state(Some(&path));
        let new = front_matter_document(&sample_metadata(), "Body");
        write_or_print_output(&cli, &new, output_state, &Progress::new(false))
            .expect("failed to handle output");

        let saved = std::fs::read_to_string(&path).expect("failed to read output file");
        assert_eq!(saved, new);

        let _ = std::fs::remove_dir_all(&dir);
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

    // --- selector_evaluation_error テスト ---

    #[test]
    fn selector_error_sentinel_is_detected() {
        // 番兵プレフィックス付きの評価結果は例外メッセージとして判別される。
        let value = format!("{SELECTOR_ERROR_SENTINEL}'div:has(' is not a valid selector.");
        assert_eq!(
            selector_evaluation_error(&value),
            Some("'div:has(' is not a valid selector."),
        );
    }

    #[test]
    fn selector_error_sentinel_absent_for_normal_results() {
        // 正規の抽出結果(HTML・空文字)はエラー扱いにならない。
        assert_eq!(selector_evaluation_error("<div>ok</div>"), None);
        assert_eq!(selector_evaluation_error(""), None);
    }

    #[test]
    fn selector_error_sentinel_survives_js_escaping() {
        // 番兵は JS 文字列リテラルとして埋め込まれるため、NUL が \u0000 に
        // エスケープされ、評価時に元の番兵文字列へ復元される表現であること。
        assert_eq!(
            escape_js_string(SELECTOR_ERROR_SENTINEL),
            r#""\u0000get-md-selector-error\u0000""#,
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
        assert!(!cli.front_matter);
        assert!(cli.meta.is_empty());
        assert!(!build_launch_options(&cli).ignore_certificate_errors);
    }

    #[test]
    fn cli_help_flags_follow_standard_conventions() {
        for flag in ["--help", "-h"] {
            let error = Cli::try_parse_from(["get-md", flag])
                .err()
                .expect("ヘルプ表示は正常終了値ではなく clap の表示結果を返す");
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            assert!(error.to_string().contains("Usage: get-md"));
        }
    }

    #[test]
    fn cli_version_flags_follow_standard_conventions() {
        for flag in ["--version", "-V"] {
            let error = Cli::try_parse_from(["get-md", flag])
                .err()
                .expect("バージョン表示は正常終了値ではなく clap の表示結果を返す");
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
            assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
        }
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
        // timeout 45 + wait 2（既定） + バッファ 30
        assert_eq!(options.idle_browser_timeout, Duration::from_secs(77));
        assert!(options.ignore_certificate_errors);
    }

    #[test]
    fn launch_options_include_wait_in_idle_timeout() {
        // --wait は thread::sleep で CDP 無通信の時間を作るため、
        // idle_browser_timeout に算入しないと待機中に接続が切れる。
        let cli = Cli::try_parse_from([
            "get-md",
            "https://example.com",
            "--wait",
            "45",
            "--timeout",
            "5",
        ])
        .unwrap();

        let options = build_launch_options(&cli);
        assert_eq!(options.idle_browser_timeout, Duration::from_secs(80));
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
        assert_eq!(idle_browser_timeout(60, 0), Duration::from_secs(90));
    }

    #[test]
    fn idle_browser_timeout_adds_wait_seconds() {
        assert_eq!(idle_browser_timeout(60, 45), Duration::from_secs(135));
    }

    #[test]
    fn idle_browser_timeout_saturates_on_overflow() {
        assert_eq!(
            idle_browser_timeout(u64::MAX, 0),
            Duration::from_secs(u64::MAX),
        );
    }

    #[test]
    fn idle_browser_timeout_saturates_on_wait_overflow() {
        assert_eq!(
            idle_browser_timeout(u64::MAX, u64::MAX),
            Duration::from_secs(u64::MAX),
        );
    }

    #[test]
    fn escape_unicode_selector() {
        assert_eq!(escape_js_string(".日本語"), r#"".日本語""#);
    }

    #[test]
    fn escape_tab_character() {
        // タブ文字は \t にエスケープする
        assert_eq!(escape_js_string("a\tb"), r#""a\tb""#);
    }

    #[test]
    fn escape_single_quotes_passthrough() {
        assert_eq!(escape_js_string("div[data-x='y']"), r#""div[data-x='y']""#);
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

    // escape_js_string の追加テスト

    #[test]
    fn escape_null_byte() {
        // NULバイトは \u0000 にエスケープする(CDP プロトコル層の終端誤認を回避)
        assert_eq!(escape_js_string("a\0b"), r#""a\u0000b""#);
    }

    #[test]
    fn escape_other_control_characters() {
        // 0x01-0x1F の制御文字は \uXXXX にエスケープする
        assert_eq!(escape_js_string("a\u{01}b"), r#""a\u0001b""#);
        assert_eq!(escape_js_string("a\u{1f}b"), r#""a\u001fb""#);
        // 0x20 (スペース) はそのまま
        assert_eq!(escape_js_string("a b"), r#""a b""#);
    }

    // escape_js_string: 複数のエスケープ対象が連続するケース

    #[test]
    fn escape_consecutive_backslashes_and_quotes() {
        assert_eq!(escape_js_string(r#"\""#), r#""\\\"""#);
    }

    // idle_browser_timeout: ゼロ秒のタイムアウト

    #[test]
    fn idle_browser_timeout_zero() {
        assert_eq!(idle_browser_timeout(0, 0), Duration::from_secs(30));
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

    // --- escape_js_string 追加テスト ---

    #[test]
    fn escape_js_string_empty() {
        assert_eq!(escape_js_string(""), "\"\"");
    }

    #[test]
    fn escape_js_string_only_backslashes() {
        assert_eq!(escape_js_string(r"\\"), r#""\\\\""#);
    }

    #[test]
    fn escape_js_string_with_form_feed_and_backspace() {
        // フォームフィードとバックスペースは \uXXXX にエスケープする
        let result = escape_js_string("a\x08b\x0cc");
        assert_eq!(result, r#""a\u0008b\u000cc""#);
    }

    #[test]
    fn idle_browser_timeout_typical_value() {
        // 一般的な 60 秒タイムアウトのバッファ確認
        assert_eq!(idle_browser_timeout(60, 0), Duration::from_secs(90));
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

    // --- idle_browser_timeout: 追加エッジケース ---

    #[test]
    fn idle_browser_timeout_large_value() {
        // 大きいが溢れない値
        let d = idle_browser_timeout(1000, 0);
        assert_eq!(d, Duration::from_secs(1030));
    }

    #[test]
    fn idle_browser_timeout_max_minus_30() {
        // u64::MAX - 30 は加算後にちょうど u64::MAX
        let d = idle_browser_timeout(u64::MAX - 30, 0);
        assert_eq!(d, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn idle_browser_timeout_wait_pushes_to_saturation() {
        // timeout と wait の合算がちょうど u64::MAX を超える境界
        let d = idle_browser_timeout(u64::MAX - 30, 1);
        assert_eq!(d, Duration::from_secs(u64::MAX));
    }

    // --- escape_js_string: 追加エッジケース ---

    #[test]
    fn escape_js_string_tab_escaped() {
        // タブ文字は \t にエスケープする
        assert_eq!(escape_js_string("\t"), r#""\t""#);
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

    /// HTML の断片を Markdown にする (`strip_invisible` なら見えない文字を除く)。
    fn html_to_markdown(fragments: &[&str], strip_invisible: bool) -> (String, InvisibleCount) {
        let fragments: Vec<String> = fragments.iter().map(|html| html.to_string()).collect();
        convert_html_fragments_to_markdown(&fragments, strip_invisible)
            .expect("HTML 変換が成功すること")
    }

    #[test]
    fn html_conversion_resolves_spaced_image_url_and_preserves_title() {
        let (markdown, _) = html_to_markdown(
            &[r#"<img src="asset name.png" alt="diagram" title="A title">"#],
            true,
        );

        assert_eq!(
            resolve_markdown_urls(&markdown, "https://example.com/docs/page.html"),
            r#"![diagram](<https://example.com/docs/asset%20name.png> "A title")"#
        );
    }

    #[test]
    fn html_conversion_preserves_table_cells_beyond_header_width() {
        let html = r#"
            <table>
                <thead><tr><th>First</th><th>Second</th></tr></thead>
                <tbody><tr><td>Alpha</td><td>Beta</td><td>Must survive</td></tr></tbody>
            </table>
        "#;

        assert_eq!(
            html_to_markdown(&[html], true).0,
            "| First | Second |  |\n| - | - | - |\n| Alpha | Beta | Must survive |"
        );
    }

    #[test]
    fn html_conversion_preserves_fence_longer_than_content_run() {
        assert_eq!(
            html_to_markdown(
                &["<pre><code>`````\nlet parsed = true;\n`````</code></pre>"],
                true
            )
            .0,
            "``````\n`````\nlet parsed = true;\n`````\n``````"
        );
    }

    #[test]
    fn html_conversion_preserves_quoted_code_and_resolves_following_link() {
        let html = "<pre><code>&gt; quote\n[skip](./code)\n|  a  |  b  |\n| --- | --- |</code></pre><p><a href='./page'>real</a></p>";
        let (markdown, _) = html_to_markdown(&[html], true);
        assert_eq!(
            resolve_markdown_urls(&markdown, "https://example.com/docs/"),
            "```\n> quote\n[skip](./code)\n|  a  |  b  |\n| --- | --- |\n```\n\n[real](https://example.com/docs/page)"
        );
    }

    // --- 見えない文字を除く変換の流れのテスト ---

    #[test]
    fn html_conversion_removes_decoded_character_references() {
        // htmd (html5ever) が文字参照を復号した文字も除く
        let (markdown, count) = html_to_markdown(&["<p>a&#x200B;b&#8203;c&zwj;d</p>"], true);
        assert_eq!(markdown, "abcd");
        assert_eq!(
            count,
            InvisibleCount {
                removed: 3,
                c1_controls: 0
            }
        );
    }

    #[test]
    fn html_conversion_counts_c1_controls_once_within_the_total() {
        // 木の段で除いた文字は、最後の Markdown の段では数えない (二重に数えない)
        let (markdown, count) =
            html_to_markdown(&["<p>a\u{200B}b\u{85}c</p>", "<p>\u{9F}d</p>"], true);
        assert_eq!(markdown, "abc\n\n---\n\nd");
        assert_eq!(
            count,
            InvisibleCount {
                removed: 3,
                c1_controls: 2
            }
        );
    }

    #[test]
    fn html_conversion_removes_invisible_characters_from_link_and_image_attributes() {
        let html = concat!(
            r#"<p><a href="./gu&#x200B;ide.html" title="T&#x200B;ip">Gu&#x200B;ide</a> "#,
            r#"<img src="./lo&#x200B;go.png" alt="Lo&#x200B;go" title="Ic&#x200B;on"></p>"#,
        );
        let (markdown, count) = html_to_markdown(&[html], true);
        assert_eq!(
            markdown,
            r#"[Guide](./guide.html "Tip") ![Logo](./logo.png "Icon")"#
        );
        assert_eq!(count.removed, 6);
    }

    #[test]
    fn html_conversion_removes_invisible_characters_before_url_resolution() {
        // リンク先の見えない文字は Url::join に渡る前に除く (%E2%80%8B にしない)
        let (markdown, _) = html_to_markdown(
            &[r#"<a href="./gu&#x200B;ide.html">Guide</a> <img src="./i&#x2060;mg.png" alt="">"#],
            true,
        );
        let resolved = resolve_markdown_urls(&markdown, "https://example.com/docs/page.html");
        assert_eq!(
            resolved,
            "[Guide](https://example.com/docs/guide.html) ![](https://example.com/docs/img.png)"
        );
        assert!(!resolved.contains("%E2"), "{resolved}");
    }

    #[test]
    fn html_conversion_escapes_markdown_that_follows_a_removed_leading_character() {
        // テキストノードの先頭の見えない文字を Markdown にする前に除くので、htmd が後ろの
        // 記号を escape し、見出し・コードフェンス・リストに化けない
        let (markdown, count) = html_to_markdown(
            &["<p>&#x200B;# Not a heading</p><p>&#xFEFF;~~~</p><p>&#x200B;1. Not a list</p>"],
            true,
        );
        assert_eq!(markdown, "\\# Not a heading\n\n\\~~~\n\n1\\. Not a list");
        assert_eq!(count.removed, 3);
    }

    #[test]
    fn html_conversion_collapses_whitespace_around_removed_characters() {
        assert_eq!(html_to_markdown(&["<p>a \u{200B} b</p>"], true).0, "a b");
    }

    #[test]
    fn html_conversion_removes_invisible_characters_inside_code() {
        let (markdown, count) = html_to_markdown(
            &[
                "<pre><code class=\"language-rust\">let x&#x200B; = 1;&#x202E;</code></pre><p><code>a&#x200B;b</code></p>",
            ],
            true,
        );
        assert_eq!(markdown, "```rust\nlet x = 1;\n```\n\n`ab`");
        assert_eq!(count.removed, 3);
    }

    #[test]
    fn html_conversion_keeps_emoji_sequences_and_visible_spaces() {
        let html = "<p>👨&#x200D;👩&#x200D;👧 🏴&#xE0067;&#xE0062;&#xE0073;&#xE0063;&#xE0074;&#xE007F; ❤&#xFE0F; 葛&#xE0100; a&nbsp;b　c</p>";
        let (markdown, count) = html_to_markdown(&[html], true);
        assert_eq!(
            markdown,
            "👨\u{200D}👩\u{200D}👧 🏴\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F} ❤\u{FE0F} 葛\u{E0100} a\u{A0}b\u{3000}c"
        );
        assert_eq!(count, InvisibleCount::default());
    }

    #[test]
    fn html_conversion_does_not_count_contents_that_htmd_drops() {
        let (markdown, count) = html_to_markdown(
            &[
                "<script>var s = \"\u{200B}\";</script><style>p::after{content:\"\u{200B}\"}</style><p>x</p>",
            ],
            true,
        );
        assert_eq!(markdown, "x");
        assert_eq!(count, InvisibleCount::default());
    }

    #[test]
    fn html_conversion_keeps_invisible_characters_with_keep_invisible() {
        let html = r#"<p>a&#x200B;b&#x202E;c <a href="./x&#x200B;.html">l&#xFEFF;ink</a></p>"#;
        let (markdown, count) = html_to_markdown(&[html], false);
        assert_eq!(
            markdown,
            "a\u{200B}b\u{202E}c [l\u{FEFF}ink](./x\u{200B}.html)"
        );
        assert_eq!(count, InvisibleCount::default());
    }

    // --- 見えない要素を除く抽出のテスト (Chrome が要らない部分) ---

    #[test]
    fn cli_keep_flags_default_to_removing() {
        let cli = Cli::try_parse_from(["get-md", "https://example.com"]).unwrap();
        assert!(!cli.keep_hidden);
        assert!(!cli.keep_invisible);

        let cli = Cli::try_parse_from(["get-md", "https://example.com", "--keep-hidden"]).unwrap();
        assert!(cli.keep_hidden);
        assert!(!cli.keep_invisible);

        let cli =
            Cli::try_parse_from(["get-md", "https://example.com", "--keep-invisible"]).unwrap();
        assert!(!cli.keep_hidden);
        assert!(cli.keep_invisible);
    }

    #[test]
    fn cli_help_lists_keep_options() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--keep-hidden"), "{help}");
        assert!(help.contains("--keep-invisible"), "{help}");
    }

    #[test]
    fn extraction_script_passes_escaped_arguments() {
        let script = extraction_script("a[title=\"x\"]\n", true);
        assert!(script.starts_with("(\n"), "{script}");
        assert!(script.contains(EXTRACT_SCRIPT));
        assert!(
            script.ends_with(r#")("a[title=\"x\"]\n", true, "\u0000get-md-selector-error\u0000")"#),
            "{script}"
        );
        assert!(extraction_script("main", false).contains(r#")("main", false, "#));
    }

    #[test]
    fn parse_selector_extraction_reads_fragments_and_counts() {
        let extraction = parse_selector_extraction(
            "main",
            r#"{"matched":2,"fragments":["<main>a</main>"],"hiddenElements":1,"hiddenTexts":2,"fadedChars":30,"keptChars":1}"#,
        )
        .unwrap();
        assert_eq!(extraction.matched, 2);
        assert_eq!(extraction.fragments, vec!["<main>a</main>"]);

        let mut hidden = HiddenRemovals::default();
        hidden.add(&extraction);
        hidden.add(&extraction);
        assert_eq!(
            hidden,
            HiddenRemovals {
                elements: 2,
                texts: 4,
                faded_chars: 60,
                kept_chars: 2
            }
        );

        // --keep-hidden の結果は数を持たない
        let kept =
            parse_selector_extraction("main", r#"{"matched":1,"fragments":["<main></main>"]}"#)
                .unwrap();
        assert_eq!(kept.hidden_elements, 0);
        assert_eq!(kept.fragments.len(), 1);
    }

    #[test]
    fn parse_selector_extraction_reports_invalid_selector() {
        let value = format!("{SELECTOR_ERROR_SENTINEL}'[' is not a valid selector");
        let err = parse_selector_extraction("[", &value).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Invalid CSS selector '[': '[' is not a valid selector"
        );
    }

    #[test]
    fn parse_selector_extraction_reports_visibility_errors_with_keep_hidden_hint() {
        let err = parse_selector_extraction("main", r#"{"error":"boom"}"#).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("'main'"), "{message}");
        assert!(message.contains("boom"), "{message}");
        assert!(message.contains("--keep-hidden"), "{message}");
    }

    #[test]
    fn parse_selector_extraction_rejects_unreadable_results() {
        // スクリプトが値を返さなかった (例外が外へ出た) ときは、一致 0 件とみなさずにエラーにする
        assert!(parse_selector_extraction("main", "").is_err());
        assert!(parse_selector_extraction("main", "<main></main>").is_err());
    }

    #[test]
    fn removal_summary_lists_only_nonzero_counts() {
        assert_eq!(removal_summary(&RemovalReport::default()), None);

        let only_invisible = RemovalReport {
            invisible: InvisibleCount {
                removed: 12,
                c1_controls: 0,
            },
            ..RemovalReport::default()
        };
        assert_eq!(
            removal_summary(&only_invisible).as_deref(),
            Some("見えない文字 12 個を除いた")
        );

        let all = RemovalReport {
            hidden: HiddenRemovals {
                elements: 3,
                texts: 2,
                faded_chars: 10,
                kept_chars: 100,
            },
            invisible: InvisibleCount {
                removed: 12,
                c1_controls: 4,
            },
        };
        assert_eq!(
            removal_summary(&all).as_deref(),
            Some(
                "隠れた要素 3 個・隠れたテキスト 2 か所・見えない文字 12 個 (うち C1 制御文字 4 個) を除いた"
            )
        );
    }

    #[test]
    fn hidden_content_hint_appears_only_when_faded_text_dominates() {
        let hidden = |faded_chars, kept_chars| HiddenRemovals {
            elements: 1,
            texts: 0,
            faded_chars,
            kept_chars,
        };
        assert!(hidden_content_hint(&hidden(0, 0)).is_none());
        assert!(hidden_content_hint(&hidden(99, 100)).is_none());
        assert!(
            hidden_content_hint(&hidden(100, 100))
                .is_some_and(|hint| hint.contains("--keep-hidden"))
        );
        assert!(hidden_content_hint(&hidden(5, 0)).is_some());
    }
}
