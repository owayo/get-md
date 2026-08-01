mod progress;

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

type MainResponseStatus = Arc<Mutex<Option<u32>>>;

struct BrowserPage {
    browser: Browser,
    tab: Arc<Tab>,
    main_response_status: MainResponseStatus,
}

struct OutputWriteState {
    old_content: Option<Vec<u8>>,
    file_existed_before: bool,
    had_unstaged_changes_before: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut progress = Progress::new(!cli.quiet);
    let selectors = prepare_selectors(&cli.selector);

    let BrowserPage {
        browser: _browser,
        tab,
        main_response_status,
    } = launch_browser(&cli, &mut progress)?;
    load_page(&tab, &cli.url, cli.wait, &mut progress)?;
    validate_http_status(&main_response_status, &cli.url)?;

    let html_fragments = extract_html_fragments(&tab, &selectors, &mut progress)?;
    let markdown = convert_html_fragments(&tab, &html_fragments, &cli.url, &mut progress)?;
    let output_text = finalize_output_text(markdown, cli.output.is_some());
    let output_state = capture_output_write_state(cli.output.as_deref());
    write_or_print_output(&cli, &output_text, output_state, &progress)?;

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
/// HTML パーサは文書中の U+0000 を U+FFFD に置換するため、実際の outerHTML が
/// このプレフィックスで始まることはなく、正規の抽出結果とは衝突しない。
const SELECTOR_ERROR_SENTINEL: &str = "\u{0}get-md-selector-error\u{0}";

/// セレクタ評価結果からエラー番兵を判別し、エラーであれば例外メッセージを返す。
fn selector_evaluation_error(value: &str) -> Option<&str> {
    value.strip_prefix(SELECTOR_ERROR_SENTINEL)
}

fn extract_html_fragments(
    tab: &Tab,
    selectors: &[String],
    progress: &mut Progress,
) -> Result<Vec<String>> {
    // セレクタに一致した要素の HTML を抽出する
    progress.spinner("Extracting HTML elements...");
    let mut html_fragments = Vec::new();
    for selector in selectors {
        progress.set_message(&format!("Extracting selector '{}'...", selector));

        // 一致した全要素の outerHTML を取得する。headless_chrome の evaluate は
        // exceptionDetails を検査せず例外を空値として返すため、無効なセレクタ等の
        // 例外は JS 側で捕捉して番兵付きメッセージにし、「マッチ 0 件」と区別する。
        let js = format!(
            r#"(() => {{
                try {{
                    const els = document.querySelectorAll({selector});
                    return Array.from(els).map(el => el.outerHTML).join('\n');
                }} catch (err) {{
                    return {sentinel} + String(err && err.message ? err.message : err);
                }}
            }})()"#,
            selector = escape_js_string(selector),
            sentinel = escape_js_string(SELECTOR_ERROR_SENTINEL),
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

        if let Some(message) = selector_evaluation_error(&html) {
            bail!("Invalid CSS selector '{}': {}", selector, message);
        }

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

    Ok(html_fragments)
}

fn convert_html_fragments(
    tab: &Tab,
    html_fragments: &[String],
    fallback_base_url: &str,
    progress: &mut Progress,
) -> Result<String> {
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
    for html in html_fragments {
        let md = converter
            .convert(html)
            .context("Failed to convert HTML to Markdown")?;
        md_parts.push(md);
    }

    let base_url = document_base_url(tab).unwrap_or_else(|| fallback_base_url.to_string());
    let markdown = compact_markdown(&md_parts.join("\n\n---\n\n"));
    let markdown = resolve_markdown_urls(&markdown, &base_url);
    progress.finish("Converted to Markdown");

    Ok(markdown)
}

fn finalize_output_text(markdown: String, file_output: bool) -> String {
    // 出力内容を確定する（末尾改行を保証）
    if file_output && !markdown.ends_with('\n') {
        format!("{markdown}\n")
    } else {
        markdown
    }
}

fn capture_output_write_state(output: Option<&Path>) -> OutputWriteState {
    let old_content = output.and_then(|p| fs::read(p).ok());
    // 書き込み前にファイルの存在を記録する（書き込み後は常に exists() が true になるため）
    let file_existed_before = old_content.is_some() || output.is_some_and(|p| p.exists());
    // 削除済み tracked ファイルは書き戻し後に diff が消えるため、書き込み前の状態も保持する。
    let had_unstaged_changes_before = output.is_some_and(has_unstaged_changes);

    OutputWriteState {
        old_content,
        file_existed_before,
        had_unstaged_changes_before,
    }
}

fn write_or_print_output(
    cli: &Cli,
    output_text: &str,
    output_state: OutputWriteState,
    progress: &Progress,
) -> Result<()> {
    // --ignore-date: 日時だけの差分なら書き込みをスキップ
    let date_only_change = cli.ignore_date
        && cli.output.is_some()
        && output_state
            .old_content
            .as_ref()
            .is_some_and(|old| is_date_only_change(old, output_text.as_bytes()));

    if date_only_change {
        let path = cli.output.as_ref().unwrap();
        // 未ステージ変更があれば updated 扱い（file_status と同じ契約）
        let (icon, status) =
            if output_state.had_unstaged_changes_before || has_unstaged_changes(path) {
                ("📝", "updated")
            } else {
                ("✔", "unchanged")
            };
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
                io::stdout()
                    .lock()
                    .write_all(output_text.as_bytes())
                    .context("Failed to write output")?;
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

/// 出力内容を一時ファイル経由でアトミックに書き込む。
///
/// 同一ディレクトリに一時ファイルを作成して書き込み、`rename` で目的のパスへ
/// 置き換える。`rename` は同一ファイルシステム内ではアトミックなため、書き込み中に
/// I/O エラー（ディスク容量不足など）が発生しても既存ファイルが中途半端な状態に
/// ならない。既存ファイルがある場合はそのパーミッション（Unix のモードビット等）を
/// 引き継ぎ、出力先がシンボリックリンクのときは実体パスへ解決してリンクを保ったまま
/// 更新する。リンク先が存在せず、その親ディレクトリも未作成の場合は必要な親を作成する。
///
/// なお `rename` は新しい inode で既存ファイルを置き換えるため、出力先へのハードリンクは
/// 切れ、ACL や拡張属性（xattr）は引き継がれない。データ損失を防ぐことを優先した仕様。
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create output directory: {}", parent.display()))?;

    // シンボリックリンクは実体パスへ解決し、リンク自体を通常ファイルで置き換えない。
    let write_path = resolve_output_write_path(path)?;
    let write_parent = write_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    // リンク先が存在しないシンボリックリンクに未作成の親ディレクトリがある場合も、
    // 通常パスと同じく出力先まで作成できるよう、解決後の親も作成する。
    fs::create_dir_all(write_parent).with_context(|| {
        format!(
            "Failed to create resolved output directory: {}",
            write_parent.display()
        )
    })?;

    // 既存ファイルがあれば、書き込み権限を事前確認しつつパーミッションを保持する。
    let existing_permissions = match fs::metadata(&write_path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                bail!("Output path is not a regular file: {}", path.display());
            }
            // rename は親ディレクトリの権限だけで既存ファイルを置換できてしまうため、
            // ここで書き込み用に開いて File::create と同じ「権限がなければ失敗」契約を保つ。
            OpenOptions::new()
                .write(true)
                .open(&write_path)
                .with_context(|| {
                    format!("Failed to open output file for writing: {}", path.display())
                })?;
            Some(metadata.permissions())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to read output file metadata: {}", path.display())
            });
        }
    };

    let (tmp_path, mut tmp_file) = create_temp_file(write_parent)?;
    // 書き込み失敗時に一時ファイルを残さないようガードする。
    let mut guard = TempFileGuard {
        path: tmp_path,
        persisted: false,
    };

    // 内容を書き込む前にパーミッションを揃える。書き込み後に絞ると、既存ファイルが
    // 制限モード(0600 等)の場合に新しい内容が一時的に緩いモードで読める窓ができる。
    // 開いたファイルディスクリプタへの書き込み権限は open 時に確定しており、
    // 先に読み取り専用へ変更しても後続の write_all は失敗しない。
    if let Some(permissions) = existing_permissions {
        tmp_file.set_permissions(permissions).with_context(|| {
            format!(
                "Failed to preserve output file permissions: {}",
                path.display()
            )
        })?;
    }

    tmp_file.write_all(bytes).with_context(|| {
        format!(
            "Failed to write temporary output file: {}",
            guard.path.display()
        )
    })?;

    tmp_file.sync_all().with_context(|| {
        format!(
            "Failed to sync temporary output file: {}",
            guard.path.display()
        )
    })?;
    drop(tmp_file);

    fs::rename(&guard.path, &write_path).with_context(|| {
        format!(
            "Failed to replace output file atomically: {}",
            path.display()
        )
    })?;
    guard.persisted = true;

    // rename 後に親ディレクトリを best-effort で同期する（失敗してもデータは置換済み）。
    sync_parent_dir(write_parent);
    Ok(())
}

/// 出力先がシンボリックリンクの場合は実体パスへ解決する。
///
/// `File::create` はリンクをたどって実体（リンク先が未作成でも、その親が存在すれば
/// 新規作成）を更新するため、`rename` でも同じ挙動に揃える。`canonicalize` はリンク先が
/// 存在しないと失敗するので使わず、`read_link` でリンクチェーンを手繰り、最終的なリンク先
/// （未作成でも可）を返す。リンクでないパスはそのまま返す。リンクのループや過度に深い
/// チェーンは打ち切ってエラーにする。
fn resolve_output_write_path(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    // シンボリックリンクのループや極端に深いチェーンを防ぐため反復回数を制限する。
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let target = fs::read_link(&current).with_context(|| {
                    format!("Failed to read output symlink: {}", current.display())
                })?;
                // 相対リンクはリンクのあるディレクトリを基準に解決する。
                current = if target.is_absolute() {
                    target
                } else {
                    current
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(target)
                };
            }
            // リンクでない通常パス、または最終リンク先が未作成（dangling）なら
            // そのパスへ書く（File::create と同じく実体を新規作成・更新する）。
            Ok(_) => return Ok(current),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(current),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read output path metadata: {}", current.display())
                });
            }
        }
    }
    bail!(
        "Output symlink chain is too deep (possible loop): {}",
        path.display()
    )
}

/// 指定ディレクトリ内に一意な一時ファイルを排他的に作成する。
///
/// プロセス ID・時刻・連番で名前を一意化し、衝突時は別名で再試行する。
fn create_temp_file(parent: &Path) -> Result<(PathBuf, File)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..1000u32 {
        let path = parent.join(format!(
            ".get-md-{}-{nanos}-{attempt}.tmp",
            std::process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to create temporary output file: {}", path.display())
                });
            }
        }
    }

    bail!(
        "Failed to allocate a unique temporary output file in {}",
        parent.display()
    )
}

/// 一時ファイルの後始末を保証するガード。
///
/// `rename` 成功前にエラーや早期リターンで処理が中断しても、`Drop` で一時ファイルを
/// 削除して残骸を残さない。
struct TempFileGuard {
    path: PathBuf,
    persisted: bool,
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.persisted {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// `rename` 後に親ディレクトリを best-effort で同期する。
///
/// ディレクトリエントリの更新を fsync で永続化してクラッシュ耐性を高めるが、データ自体は
/// `rename` 完了時点で既存ファイルを壊さず置き換え済みなので、fsync の失敗は無視する
/// （durability の追加保証が得られないだけ）。
#[cfg(unix)]
fn sync_parent_dir(parent: &Path) {
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent_dir(_parent: &Path) {}

/// ファイル出力のステータスを判定する。
///
/// `file_existed_before` は書き込み前に記録したファイルの存在状態。
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

    // glob メタ文字(`*` `?` `[...]`)を含むパスが pathspec として展開され、
    // 同パターンにマッチする別ファイルの未ステージ変更を誤検出しないよう
    // literal magic を付けてリテラル一致に固定する。
    // `git -C` 後の作業ディレクトリを基準にした相対パスを渡す。絶対パスへ
    // pathspec magic を直接連結すると、Windows のドライブレターを含むパスを
    // Git がリポジトリ内 pathspec として解釈できない。
    let Ok(relative_path) = absolute_path.strip_prefix(git_dir) else {
        return false;
    };
    let mut literal_pathspec = OsString::from(":(literal)");
    literal_pathspec.push(relative_path.as_os_str());

    Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["diff", "--name-only", "--"])
        .arg(literal_pathspec)
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
            if in_fenced_code_block {
                // フェンス内では info string 付きのマーカーを閉じ扱いしない。
                // 閉じフェンスはマーカー以降が空白/タブのみでなければならない。
                if is_closing_fence_line_after_indent(line, fence_char, fence_len) {
                    in_fenced_code_block = false;
                    fence_char = '\0';
                    fence_len = 0;
                    table_state = TableState::Outside;
                    return line.to_string();
                }
                return line.to_string();
            }
            if let Some((marker, marker_len)) = fence_marker_after_indent(line) {
                table_state = TableState::Outside;
                in_fenced_code_block = true;
                fence_char = marker;
                fence_len = marker_len;
                return line.to_string();
            }

            // 4 スペース以上のインデント行は CommonMark のインデントコードブロックなので
            // テーブル扱いせずに行をそのまま保持する。`line.trim()` で先頭空白を落として
            // しまうとコード内容が壊れる。
            if strip_fence_indent(line).is_none() {
                table_state = TableState::Outside;
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
    if len < 3 {
        return None;
    }

    // CommonMark §4.5: backtick fence の info string にはバッククォートを含められない
    // (含めるとインラインコードがフェンス開始と誤認されてしまうため)。
    // tilde fence にはこの制約は無い。
    if marker == '`' && line[len..].contains('`') {
        return None;
    }

    Some((marker, len))
}

/// CommonMark のフェンス用インデント（最大 3 スペース）を取り除く。
///
/// 4 スペース以上の行はインデントコードブロックなので、フェンス開始/終了として
/// 扱わない。タブは列幅計算が必要になるため、ここでは安全側に倒して除外する。
fn strip_fence_indent(line: &str) -> Option<&str> {
    let mut spaces = 0usize;
    for (idx, ch) in line.char_indices() {
        match ch {
            ' ' if spaces < 3 => spaces += 1,
            ' ' | '\t' => return None,
            _ => return Some(&line[idx..]),
        }
    }
    Some("")
}

fn fence_marker_after_indent(line: &str) -> Option<(char, usize)> {
    strip_fence_indent(line).and_then(fence_marker)
}

fn is_closing_fence_line_after_indent(line: &str, marker: char, min_len: usize) -> bool {
    let Some(rest) = strip_fence_indent(line) else {
        return false;
    };
    is_closing_fence_line(rest, marker, min_len)
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
    strip_fence_blockquote_markers(line).and_then(fence_marker)
}

/// 行頭のインデントとブロッククォート記号 (`>`) を取り除いた残りを返す。
///
/// 任意インデントを許容する旧来の剥がし処理として、直接テストで境界を固定する。
#[cfg(test)]
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

/// フェンス検出用に CommonMark のインデント規則を守ってブロッククォート記号を取り除く。
fn strip_fence_blockquote_markers(line: &str) -> Option<&str> {
    let mut rest = strip_fence_indent(line)?;
    while let Some(after_marker) = rest.strip_prefix('>') {
        rest = after_marker.strip_prefix(' ').unwrap_or(after_marker);
        rest = strip_fence_indent(rest)?;
    }
    Some(rest)
}

/// URL 解決時の段落境界となる空行か判定する。
///
/// 通常の空行に加え、ブロッククォート記号だけの行も、そのクォート内では
/// 空行として扱う。
fn is_blank_line_for_link_scan(line: &str) -> bool {
    line.chars().all(|c| matches!(c, ' ' | '\t' | '\r' | '>'))
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
    // 開いているインラインコードスパンのバッククォート列の長さ。0 ならコード外。
    // コードスパン内の `|` はセル区切りとして扱わない（CommonMark/GFM 仕様）。
    let mut inline_code_len = 0usize;
    let mut i = 0;

    while i < inner.len() {
        let rest = &inner[i..];

        if rest.starts_with('`') {
            let tick_len = rest.chars().take_while(|c| *c == '`').count();
            if inline_code_len == 0 {
                let escaped = backslash_run % 2 == 1;
                // 同じ長さの閉じバッククォート列が後続にあるときだけインラインコードとして開く。
                if !escaped && has_matching_inline_code_closer(inner, i + tick_len, tick_len) {
                    inline_code_len = tick_len;
                }
            } else if tick_len == inline_code_len {
                inline_code_len = 0;
            }
            i += tick_len;
            backslash_run = 0;
            continue;
        }

        let ch = rest.chars().next().expect("cursor は文字境界上にある");
        let ch_len = ch.len_utf8();

        // インラインコード内は内容を解釈しない（`|` も `\` もリテラル扱い）。
        if inline_code_len > 0 {
            i += ch_len;
            backslash_run = 0;
            continue;
        }

        if ch == '\\' {
            backslash_run += 1;
            i += ch_len;
            continue;
        }

        let escaped = backslash_run % 2 == 1;
        if ch == '|' && !escaped {
            cells.push(&inner[start..i]);
            start = i + 1;
        }

        backslash_run = 0;
        i += ch_len;
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

    // ブロック構造（コード領域・空行）は走査のたびに作り直せないため先に 1 パスで求める。
    let blocks = MarkdownBlockMap::build(md);
    let mut result = String::with_capacity(md.len());
    let mut cursor = 0usize;
    // ループをまたいで、cursor 以前のコード領域外で未閉鎖の `[` を引き継ぐ。
    // これにより `[![inner](img)](outer)` のように外側 `[` が cursor より前に
    // ある場合でも、後続走査で外側リンクを認識できる。
    let mut pending_open_brackets: usize = 0;

    while let (Some(open), open_count_at_link) =
        find_next_link_candidate(md, cursor, pending_open_brackets, &blocks)
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

/// URL 解決の走査における行の分類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LinkScanLineKind {
    /// 空行（段落境界）。ブロッククォート記号だけの行も含む。
    Blank,
    /// コード領域の行。フェンスコードブロックとインデントコードブロックの両方。
    Code,
    /// 通常の行。リンクの URL 解決対象。
    Normal,
}

/// 1 行分の分類結果。
struct LinkScanLine {
    /// 行頭のバイトオフセット
    start: usize,
    /// 次の行頭のバイトオフセット（最終行では入力全体の長さ）
    next_start: usize,
    kind: LinkScanLineKind,
}

/// Markdown を行単位で分類したブロック構造マップ。
///
/// URL 解決は `find_next_link_candidate` を何度も呼び直し、リンクを 1 つ処理する
/// たびにカーソルが行をまたいで飛ぶ。走査のたびにブロック状態を組み立て直すと、
/// 文書先頭からの累積でしか決まらないリストの入れ子状態を復元できないため、
/// 文書全体を先に 1 パスで分類しておき、走査側は行頭オフセットで引くだけにする。
///
/// インデントコードブロックの判定基準は、CommonMark に従って
/// 「その行を含むリスト項目の内容インデント + 4」とする。単純に
/// 「行頭 4 スペース以上」で判定すると、htmd が出力するネストしたリスト
/// （3 段目で 4 スペース以上になる）をコードと誤判定し、
/// リスト内のリンクが絶対 URL に解決されなくなる。
struct MarkdownBlockMap {
    lines: Vec<LinkScanLine>,
}

#[derive(Default)]
struct MarkdownBlockState {
    /// 開いているリスト項目の内容インデント（外側から内側の順）
    list_indents: Vec<usize>,
    /// 開いているフェンス: (マーカー文字, マーカー長, 属するコンテナの内容インデント)
    open_fence: Option<(char, usize, usize)>,
    blockquote_depth: usize,
}

impl MarkdownBlockState {
    fn classify_line(&mut self, line: &str) -> LinkScanLineKind {
        if is_blank_line_for_link_scan(line) {
            // 空行ではリストもフェンスも閉じない
            // （loose list は項目の間に空行が入り、フェンス内にも空行は現れる）
            return if self.open_fence.is_some() {
                LinkScanLineKind::Code
            } else {
                LinkScanLineKind::Blank
            };
        }

        let (body, depth) = strip_blockquote_prefix_for_scan(line);
        if depth != self.blockquote_depth {
            // ブロッククォートの段数が変わったら、属していたコンテナの状態は引き継がない
            self.blockquote_depth = depth;
            self.list_indents.clear();
            self.open_fence = None;
        }

        match split_leading_spaces(body) {
            // タブはインデント幅の計算が必要なため、安全側でコード扱いにする
            None => LinkScanLineKind::Code,
            Some((indent, content)) => self.classify_indented_line(indent, content),
        }
    }

    fn classify_indented_line(&mut self, indent: usize, content: &str) -> LinkScanLineKind {
        // フェンスを含むコンテナ（リスト項目）より浅い行に来たら、
        // そのコンテナごとフェンスも閉じる。
        if self
            .open_fence
            .is_some_and(|(_, _, fence_base)| indent < fence_base)
        {
            self.open_fence = None;
        }

        if let Some((marker, marker_len, fence_base)) = self.open_fence {
            // フェンス内。閉じフェンスに当たればここで閉じるが、その行自体もコード。
            // 閉じフェンスの追加インデントはコンテナ基準で最大 3 スペースまで。
            if indent <= fence_base + 3 && is_closing_fence_line(content, marker, marker_len) {
                self.open_fence = None;
            }
            return LinkScanLineKind::Code;
        }

        self.classify_normal_line(indent, content)
    }

    fn classify_normal_line(&mut self, indent: usize, content: &str) -> LinkScanLineKind {
        // 内容インデントより浅い行に来たリスト項目は閉じている
        while self.list_indents.last().is_some_and(|top| indent < *top) {
            self.list_indents.pop();
        }
        let base = self.list_indents.last().copied().unwrap_or(0);
        if indent >= base + 4 {
            return LinkScanLineKind::Code;
        }
        if let Some((fence_char, fence_len)) = fence_marker(content) {
            self.open_fence = Some((fence_char, fence_len, base));
            return LinkScanLineKind::Code;
        }

        // `* * *` のようなテーマ区切りはリスト項目ではない
        if !is_thematic_break(content)
            && let Some(content_indent) = list_item_content_indent(indent, content)
        {
            self.list_indents.push(content_indent);

            // htmd はリスト項目先頭のコードブロックを
            // `* ``` ` のようにマーカーと同じ行から出力する。
            let item_content = content.get(content_indent - indent..);
            if let Some((fence_char, fence_len)) = item_content.and_then(fence_marker) {
                self.open_fence = Some((fence_char, fence_len, content_indent));
                return LinkScanLineKind::Code;
            }
        }

        LinkScanLineKind::Normal
    }
}

impl MarkdownBlockMap {
    fn build(md: &str) -> Self {
        let mut lines = Vec::new();
        let mut state = MarkdownBlockState::default();
        let mut start = 0usize;

        loop {
            let line_end = md[start..]
                .find('\n')
                .map(|offset| start + offset)
                .unwrap_or(md.len());
            let next_start = if line_end < md.len() {
                line_end + 1
            } else {
                md.len()
            };
            // 行末の CR は line ending の一部なので、分類前に落とす
            let raw_line = &md[start..line_end];
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);

            let kind = state.classify_line(line);

            lines.push(LinkScanLine {
                start,
                next_start,
                kind,
            });

            if line_end >= md.len() {
                break;
            }
            start = next_start;
        }

        Self { lines }
    }

    /// 指定した行頭オフセットの分類を返す。行頭以外のオフセットでは None。
    fn line_starting_at(&self, offset: usize) -> Option<&LinkScanLine> {
        self.lines
            .binary_search_by_key(&offset, |line| line.start)
            .ok()
            .map(|index| &self.lines[index])
    }
}

/// 行頭のブロッククォート記号 (`>`) を取り除き、(残りの本文, ネスト段数) を返す。
///
/// 記号の前のインデントは CommonMark に合わせて最大 3 スペースまで許容する。
/// 4 スペース以上インデントされている場合はブロッククォートではなくインデント
/// コードの領域なので、そこで打ち切って残りをそのまま返す。
fn strip_blockquote_prefix_for_scan(line: &str) -> (&str, usize) {
    let mut rest = line;
    let mut depth = 0usize;

    while let Some(after_indent) = strip_fence_indent(rest) {
        let Some(after_marker) = after_indent.strip_prefix('>') else {
            break;
        };
        depth += 1;
        rest = after_marker.strip_prefix(' ').unwrap_or(after_marker);
    }

    (rest, depth)
}

/// 行頭の半角スペースを数え、(スペース数, 残りの本文) を返す。
///
/// タブが現れた場合はタブストップを考慮したインデント幅の計算が必要になるため
/// None を返し、呼び出し側は安全側（コード扱い）にフォールバックする。
fn split_leading_spaces(line: &str) -> Option<(usize, &str)> {
    for (index, ch) in line.char_indices() {
        match ch {
            ' ' => continue,
            '\t' => return None,
            _ => return Some((index, &line[index..])),
        }
    }
    Some((line.len(), ""))
}

/// 行がリスト項目の開始なら、その項目の内容インデントを返す。
///
/// `indent` はブロッククォート記号を除いた後の行頭スペース数、`content` は
/// そのインデントより後ろの本文。
/// CommonMark では、マーカー直後の空白が 1〜4 個ならその直後が内容の開始位置、
/// 5 個以上（残りがインデントコードになる）や空のリスト項目では
/// 「マーカー + 空白 1 個」の位置が内容の開始位置になる。
fn list_item_content_indent(indent: usize, content: &str) -> Option<usize> {
    let marker_len = list_marker_len(content)?;
    let after_marker = &content[marker_len..];
    let spaces = after_marker.chars().take_while(|c| *c == ' ').count();

    let offset = if after_marker.is_empty() || after_marker.starts_with('\t') {
        // 空のリスト項目、またはタブ区切り
        marker_len + 1
    } else if spaces == 0 {
        // `*foo` や `1.foo` はリストマーカーではない
        return None;
    } else if spaces <= 4 && spaces < after_marker.len() {
        marker_len + spaces
    } else {
        // 空白が 5 個以上、または空白だけで終わる行
        marker_len + 1
    };

    Some(indent + offset)
}

/// テーマ区切り（`***` `---` `___` を 3 個以上、間の空白は自由）か判定する。
///
/// `* * *` や `- - -` は行頭がリストマーカーと同じ文字で始まるが、CommonMark では
/// テーマ区切りでありリスト項目ではない。リストの内容インデントを積んでしまうと、
/// 後続の 4 スペース行をインデントコードと判定できなくなる。
fn is_thematic_break(content: &str) -> bool {
    let mut marker: Option<char> = None;
    let mut count = 0usize;

    for ch in content.chars() {
        match ch {
            ' ' | '\t' => continue,
            '*' | '-' | '_' => {
                if *marker.get_or_insert(ch) != ch {
                    return false;
                }
                count += 1;
            }
            _ => return false,
        }
    }

    count >= 3
}

/// リストマーカーの長さを返す（`-` `*` `+` は 1、`12.` のような順序付きは桁数 + 1）。
///
/// CommonMark の順序付きリスト番号は最大 9 桁。
fn list_marker_len(content: &str) -> Option<usize> {
    let mut chars = content.chars();
    match chars.next()? {
        '-' | '*' | '+' => Some(1),
        first if first.is_ascii_digit() => {
            let mut digits = 1usize;
            for ch in chars {
                match ch {
                    '0'..='9' => {
                        digits += 1;
                        if digits > 9 {
                            return None;
                        }
                    }
                    '.' | ')' => return Some(digits + 1),
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// コード領域を除外しつつ、次の `](` を探す。
///
/// フェンス/インデントコードブロックと空行の位置は `blocks` から引き、
/// コード領域外で開いている `[` を前方走査でカウントする。エスケープされた
/// `\]` `\`` `\[` は正しくリテラルとして扱う。`initial_open_brackets` は
/// `start` 位置より前から引き継いだ未閉鎖 `[` の数（外側リンク対応のため）。
/// 戻り値は `(](`の位置, 検出時点の未閉鎖 `[` 数)`。
fn find_next_link_candidate(
    md: &str,
    start: usize,
    initial_open_brackets: usize,
    blocks: &MarkdownBlockMap,
) -> (Option<usize>, usize) {
    let mut cursor = start;
    let mut line_start = start == 0 || md[..start].ends_with('\n');
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
            if let Some(line) = blocks.line_starting_at(cursor) {
                match line.kind {
                    // 空行は段落境界なので、前段落で未閉鎖だった `[` を引き継がない。
                    LinkScanLineKind::Blank => open_bracket_count = 0,
                    // コードブロックも段落境界なので、その前後をリンクとして接続しない。
                    LinkScanLineKind::Code => {
                        open_bracket_count = 0;
                        cursor = line.next_start;
                        line_start = true;
                        backslash_run = 0;
                        continue;
                    }
                    LinkScanLineKind::Normal => {}
                }
            }
            line_start = false;
        }

        let rest = &md[cursor..];

        // インラインコード内は同じ長さのバッククォート列で閉じる
        if inline_code_len > 0 {
            if rest.starts_with('`') {
                let tick_len = rest.chars().take_while(|c| *c == '`').count();
                if tick_len == inline_code_len {
                    inline_code_len = 0;
                }
                cursor += tick_len;
                // バッククォート列は改行を含まないため、消費後は行頭ではない。
                // これを反映しないと、改行をまたぐインラインコードが行の途中で
                // 閉じたとき、行の残りがフェンス開始/インデントコード行として
                // 誤判定され、同一行以降のリンクが解決されなくなる。
                line_start = false;
                backslash_run = 0;
                continue;
            }
            let Some(ch) = rest.chars().next() else {
                break;
            };
            cursor += ch.len_utf8();
            // 非改行文字を消費したら行頭フラグを下ろし、行頭判定の陳腐化を防ぐ。
            line_start = ch == '\n';
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
///
/// インラインコードは段落をまたいで延びることが無いため、フェンスコードブロック
/// の開始行、または空行(段落境界)に到達した時点で「閉じ列なし」と判定する。
/// これを行わないと、未閉鎖の `` ` `` のあとに現れたフェンス内・別段落の `` ` `` を
/// 閉じと誤認してしまう。
fn has_matching_inline_code_closer(md: &str, start: usize, tick_len: usize) -> bool {
    let mut cursor = start;
    let mut line_start = start == 0 || md[..start].ends_with('\n');

    while cursor < md.len() {
        if line_start {
            let line_end = md[cursor..]
                .find('\n')
                .map(|offset| cursor + offset)
                .unwrap_or(md.len());
            let line = &md[cursor..line_end];
            // 空行（ブロッククォート記号だけの行を含む）は段落境界なので、
            // ここで探索を打ち切る。
            if is_blank_line_for_link_scan(line) {
                return false;
            }
            // フェンス開始行に到達したら、ここでインラインコード探索を打ち切る。
            // ブロッククォート内のフェンスも対象にする。
            if fence_marker_after_blockquote(line).is_some() {
                return false;
            }
            line_start = false;
        }

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
        if ch == '\n' {
            line_start = true;
        }
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
///
/// CommonMark ではリンクの構成要素(リンク先・title・その前後の空白)は
/// 空行(ブロック境界)を跨げないため、空行を検出した時点で打ち切る。
/// これを行わないと、空行の先にある無関係な `)` を閉じと誤認し、
/// リンクではない通常テキストを URL に書き換えてしまう。
fn find_link_close_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    let mut backslash_run = 0usize;
    let mut title_quote: Option<char> = None;
    let mut saw_dest_non_ws = false;
    let mut saw_sep_ws = false;
    let mut in_angle_destination = false;
    // 改行後に空白/タブ/CR のみが続いている間 true。この状態で再度改行が
    // 現れたら空行(ブロック境界)なので、山括弧内・title 内を問わず打ち切る。
    let mut blank_line_pending = false;

    for (i, c) in s.char_indices() {
        if c == '\n' {
            if blank_line_pending {
                return None;
            }
            blank_line_pending = true;
        } else if blank_line_pending && !matches!(c, ' ' | '\t' | '\r') {
            blank_line_pending = false;
        }

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
        super::find_next_link_candidate(md, start, 0, &MarkdownBlockMap::build(md)).0
    }

    /// テスト用に、指定行頭オフセットの分類を取り出す。
    fn line_kind_at(md: &str, offset: usize) -> Option<LinkScanLineKind> {
        MarkdownBlockMap::build(md)
            .line_starting_at(offset)
            .map(|line| line.kind)
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
    fn finalize_output_text_adds_newline_for_file_output() {
        assert_eq!(
            finalize_output_text("content".to_string(), true),
            "content\n"
        );
    }

    #[test]
    fn finalize_output_text_does_not_duplicate_trailing_newline() {
        assert_eq!(
            finalize_output_text("content\n".to_string(), true),
            "content\n"
        );
    }

    #[test]
    fn finalize_output_text_keeps_stdout_output_unchanged() {
        assert_eq!(
            finalize_output_text("content".to_string(), false),
            "content"
        );
    }

    #[test]
    fn capture_output_write_state_none_is_empty() {
        let state = capture_output_write_state(None);

        assert!(state.old_content.is_none());
        assert!(!state.file_existed_before);
        assert!(!state.had_unstaged_changes_before);
    }

    #[test]
    fn capture_output_write_state_reads_existing_file() {
        let dir = make_temp_dir("get-md-output-state");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, b"old content").expect("failed to write fixture file");

        let state = capture_output_write_state(Some(&path));

        assert_eq!(state.old_content.as_deref(), Some(&b"old content"[..]));
        assert!(state.file_existed_before);
        assert!(!state.had_unstaged_changes_before);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_output_write_state_missing_file_is_not_existing() {
        let dir = make_temp_dir("get-md-output-state-missing");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("missing.md");

        let state = capture_output_write_state(Some(&path));

        assert!(state.old_content.is_none());
        assert!(!state.file_existed_before);
        assert!(!state.had_unstaged_changes_before);

        let _ = std::fs::remove_dir_all(&dir);
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
        // タブ文字は \t にエスケープする
        assert_eq!(escape_js_string("a\tb"), r#""a\tb""#);
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
    fn is_table_separator_cell_accepts_dashes_and_colons() {
        // セパレータセルは `-` 単独、`:---`, `---:`, `:---:` を許容する。
        assert!(is_table_separator_cell("-"));
        assert!(is_table_separator_cell("---"));
        assert!(is_table_separator_cell(":---"));
        assert!(is_table_separator_cell("---:"));
        assert!(is_table_separator_cell(":---:"));
        // コロンだけの行はテーブル本文側でデータセルとして残す必要があるが、
        // セパレータ判定単体としては true（実際の保持判定は別関数）。
        assert!(is_table_separator_cell(":"));
    }

    #[test]
    fn is_table_separator_cell_rejects_non_separator_chars() {
        assert!(!is_table_separator_cell(""));
        assert!(!is_table_separator_cell("a"));
        assert!(!is_table_separator_cell("- - -"));
        assert!(!is_table_separator_cell("--a--"));
    }

    #[test]
    fn is_table_separator_row_accepts_all_separator_cells() {
        assert!(is_table_separator_row("| --- |"));
        assert!(is_table_separator_row("| --- | --- |"));
        assert!(is_table_separator_row("| :--- | ---: | :-: |"));
    }

    #[test]
    fn is_table_separator_row_rejects_when_any_cell_is_data() {
        // 1 つでも非セパレータセルがあれば false。
        assert!(!is_table_separator_row("| --- | data |"));
        assert!(!is_table_separator_row("| a | b |"));
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
    fn resolve_invalid_destination_url_is_preserved() {
        // URL として解釈できないリンク先は、壊さず元の文字列のまま保持する。
        let input = "[bad](http://[::1)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_invalid_angle_destination_url_is_preserved() {
        // 山括弧形式でも URL 解決に失敗した場合は山括弧形式を維持する。
        let input = "[bad](<http://[::1>)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_invalid_destination_url_preserves_title() {
        // URL 解決に失敗しても title は切り落とさず保持する。
        let input = r#"[bad](http://[::1 "title")"#;
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_does_not_cross_blank_line_for_close_paren() {
        // CommonMark ではリンク構文の構成要素は空行(ブロック境界)を跨げない。
        // 空行の先にある `)` を閉じと誤認して通常テキストを URL 化しないこと。
        let input = "Some [text]( and more.\n\nLater a closing ) here.";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_after_blank_line_inside_broken_candidate() {
        // 空行跨ぎの壊れた候補に飲み込まれず、空行後の本物のリンクを解決すること。
        let input = "[a]( x\n\n[real](./page) y )";
        let expected = "[a]( x\n\n[real](https://example.com/docs/en/page) y )";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_open_bracket_does_not_cross_blank_line() {
        // 空行の前にある未閉鎖 `[` と後段落の `](` はリンクを構成しないため、
        // 後段落の文字列を URL として書き換えないこと。
        let input = "[broken\n\ntext](./page)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_open_bracket_does_not_cross_blockquote_blank_line() {
        // ブロッククォート記号だけの行もクォート内の段落境界として扱う。
        let input = "> [broken\n>\n> text](./page)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_open_bracket_does_not_cross_fenced_code_block() {
        // フェンスコードブロック前の未閉鎖 `[` をブロック後へ引き継がないこと。
        let input = "[broken\n```text\ncode\n```\ntext](./page)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_text_may_cross_single_line_break() {
        // 空行でない単一改行はリンクテキスト内の soft break なので URL 解決を維持する。
        let input = "[first\nsecond](./page)";
        let expected = "[first\nsecond](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_backticks_across_blank_line_do_not_form_code_span() {
        // インラインコードは段落(空行)を跨げない。空行を挟んだバッククォート同士を
        // コードスパンと誤認して、後続段落のリンクを未解決のまま残さないこと。
        let input = "`a\n\nb [x](./y) c`";
        let expected = "`a\n\nb [x](https://example.com/docs/en/y) c`";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_backticks_across_blockquote_blank_line_do_not_form_code_span() {
        // `>` だけの行もブロッククォート内の段落境界。前段落の未閉鎖バッククォートで
        // 後段落のリンクをコード扱いしないこと。
        let input = "> `a\n>\n> b [x](./y) c`";
        let expected = "> `a\n>\n> b [x](https://example.com/docs/en/y) c`";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
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

    #[test]
    fn find_close_paren_stops_at_blank_line() {
        // 空行はブロック境界なので、その先の `)` は閉じとして採用しない。
        assert_eq!(find_link_close_paren("text\n\nmore )"), None);
        // 空白/タブだけの行も CommonMark の blank line として扱う。
        assert_eq!(find_link_close_paren("text\n \t\nmore )"), None);
        // CRLF の空行も同様に打ち切る。
        assert_eq!(find_link_close_paren("text\r\n\r\nmore )"), None);
    }

    #[test]
    fn find_close_paren_allows_single_newline() {
        // 単一改行(空行でない)は従来どおり許容し、閉じ `)` を返す。
        assert_eq!(find_link_close_paren("./a\ntitlepart)"), Some(13));
        // title 内の単一改行も許容する。
        assert_eq!(find_link_close_paren("./a \"line\nbreak\")"), Some(16));
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
    fn fence_marker_after_indent_allows_up_to_three_spaces() {
        assert_eq!(fence_marker_after_indent("```"), Some(('`', 3)));
        assert_eq!(fence_marker_after_indent("   ```"), Some(('`', 3)));
        assert_eq!(fence_marker_after_indent("    ```"), None);
        assert_eq!(fence_marker_after_indent("\t```"), None);
    }

    #[test]
    fn strip_fence_indent_no_indent_returns_input() {
        assert_eq!(strip_fence_indent("```"), Some("```"));
    }

    #[test]
    fn strip_fence_indent_up_to_three_spaces_allowed() {
        assert_eq!(strip_fence_indent(" ```"), Some("```"));
        assert_eq!(strip_fence_indent("  ```"), Some("```"));
        assert_eq!(strip_fence_indent("   ```"), Some("```"));
    }

    #[test]
    fn strip_fence_indent_four_spaces_rejected() {
        assert_eq!(strip_fence_indent("    ```"), None);
    }

    #[test]
    fn strip_fence_indent_leading_tab_rejected() {
        // タブは CommonMark のインデントコードブロック扱い。
        assert_eq!(strip_fence_indent("\t```"), None);
    }

    #[test]
    fn strip_fence_indent_tab_after_spaces_rejected() {
        // スペースの後にタブが混ざってもインデントコード扱いで拒否。
        assert_eq!(strip_fence_indent(" \t```"), None);
        assert_eq!(strip_fence_indent("  \t```"), None);
    }

    #[test]
    fn strip_fence_indent_empty_line_passes_through() {
        assert_eq!(strip_fence_indent(""), Some(""));
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
    fn fence_marker_backtick_info_string_with_backtick_is_rejected() {
        // CommonMark §4.5: backtick fence の info string にバッククォートを含めると
        // フェンス開始として無効になる(さもなくばインラインコード `code` がフェンスと誤認される)。
        assert_eq!(fence_marker("``` `code`"), None);
        assert_eq!(fence_marker("```foo`bar"), None);
        // info string 末尾にバッククォートがある場合も拒否
        assert_eq!(fence_marker("```rust`"), None);
    }

    #[test]
    fn fence_marker_backtick_info_string_without_backtick_is_accepted() {
        // バッククォートを含まない info string は受理する
        assert_eq!(fence_marker("```rust"), Some(('`', 3)));
        assert_eq!(fence_marker("```{.rust .numberLines}"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_tilde_info_string_with_backtick_is_accepted() {
        // tilde fence にはバッククォート禁止の制約はない。
        assert_eq!(fence_marker("~~~ `code`"), Some(('~', 3)));
        assert_eq!(fence_marker("~~~foo`bar"), Some(('~', 3)));
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
    fn resolve_url_when_backtick_fence_info_string_contains_backtick() {
        // CommonMark §4.5: backtick fence の info string にバッククォートを含む行は
        // フェンス開始ではないため、後続の [link](./page) は通常リンクとして解決される。
        let input = "``` `bad`\n[link](./page)\n```";
        let expected = "``` `bad`\n[link](https://example.com/docs/en/page)\n```";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_url_when_unclosed_inline_code_meets_fence() {
        // 未閉鎖のインラインコード `` ` `` のあとに本物のフェンス開始行があるとき、
        // インラインコード探索はフェンス境界で打ち切られるため、未閉鎖の `` ` `` は
        // リテラル扱いになる。よって `[link](./page)` は通常リンクとして解決される。
        let input = "` literal [link](./page)\n```\ncode\n```";
        let expected = "` literal [link](https://example.com/docs/en/page)\n```\ncode\n```";
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
    fn has_unstaged_changes_does_not_glob_match_sibling_files() {
        let dir = make_temp_dir("get-md-git-glob-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        // glob メタ文字 `[...]` を含む出力ファイル名(Windows でも合法)と、
        // その文字クラスにマッチしてしまう隣接ファイルを用意する。
        let glob_named = dir.join("notes [draft].md");
        let sibling = dir.join("notes d.md");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);

        std::fs::write(&glob_named, b"glob").expect("failed to write glob-named file");
        std::fs::write(&sibling, b"sibling").expect("failed to write sibling file");
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-m", "init"]);

        // 隣接ファイルだけの変更を、pathspec の glob 展開で誤検出しないこと。
        std::fs::write(&sibling, b"changed").expect("failed to update sibling file");
        assert!(!has_unstaged_changes(&glob_named));

        // 出力ファイル自身の変更は従来どおり検出されること。
        std::fs::write(&glob_named, b"changed").expect("failed to update glob-named file");
        assert!(has_unstaged_changes(&glob_named));

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

    // atomic_write のテスト

    #[test]
    fn atomic_write_creates_new_file() {
        let dir = make_temp_dir("get-md-atomic-new");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("out.md");

        atomic_write(&path, b"new content").expect("atomic_write should succeed");

        assert_eq!(std::fs::read(&path).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = make_temp_dir("get-md-atomic-overwrite");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("out.md");
        std::fs::write(&path, b"old content").expect("failed to write fixture");

        atomic_write(&path, b"new content").expect("atomic_write should succeed");

        assert_eq!(std::fs::read(&path).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_creates_missing_parent_directory() {
        let dir = make_temp_dir("get-md-atomic-parent");
        let path = dir.join("nested").join("sub").join("out.md");

        atomic_write(&path, b"content").expect("atomic_write should create parents");

        assert_eq!(std::fs::read(&path).unwrap(), b"content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_on_success() {
        let dir = make_temp_dir("get-md-atomic-cleanup");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("out.md");

        atomic_write(&path, b"content").expect("atomic_write should succeed");

        // 成功後は出力ファイルだけが残り、一時ファイル（.get-md-*.tmp）は残らない。
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["out.md".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = make_temp_dir("get-md-atomic-perms");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("out.md");
        std::fs::write(&path, b"old").expect("failed to write fixture");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("failed to set permissions");

        atomic_write(&path, b"new").expect("atomic_write should succeed");

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_symlink_and_keeps_link() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-symlink");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let target = dir.join("real.md");
        let link = dir.join("link.md");
        std::fs::write(&target, b"old").expect("failed to write target");
        symlink(&target, &link).expect("failed to create symlink");

        atomic_write(&link, b"new content").expect("atomic_write should follow symlink");

        // リンク自体は symlink のまま保持され、実体ファイルの内容が更新される。
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_through_dangling_symlink() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-dangling");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let target = dir.join("real.md");
        let link = dir.join("link.md");
        // リンク先 real.md は未作成（dangling symlink）。
        symlink(&target, &link).expect("failed to create symlink");

        atomic_write(&link, b"created via dangling link")
            .expect("atomic_write should follow a dangling symlink");

        // dangling だったリンク先が新規作成され、リンク自体は保持される。
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"created via dangling link"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_missing_parent_of_dangling_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-dangling-parent");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let target = dir.join("missing/subdir/real.md");
        let link = dir.join("link.md");
        symlink(&target, &link).expect("failed to create symlink");

        atomic_write(&link, b"created with missing target parent")
            .expect("atomic_write should create the dangling target parent");

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"created with missing target parent"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_breaks_hard_links_by_design() {
        let dir = make_temp_dir("get-md-atomic-hardlink");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let a = dir.join("a.md");
        let b = dir.join("b.md");
        std::fs::write(&a, b"shared").expect("failed to write fixture");
        std::fs::hard_link(&a, &b).expect("failed to create hard link");

        atomic_write(&a, b"updated").expect("atomic_write should succeed");

        // atomic write は新しい inode で置き換えるため hard link は切れ、b は旧内容のまま。
        // これはデータ損失防止と引き換えの意図的な仕様。
        assert_eq!(std::fs::read(&a).unwrap(), b"updated");
        assert_eq!(std::fs::read(&b).unwrap(), b"shared");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn atomic_write_rejects_directory_path() {
        // 出力先が既存ディレクトリの場合は通常ファイルではないためエラーにする。
        let dir = make_temp_dir("get-md-atomic-dir");
        let target = dir.join("subdir");
        std::fs::create_dir_all(&target).expect("failed to create dir");

        let result = atomic_write(&target, b"content");
        assert!(result.is_err());

        // ディレクトリは壊されずに残る。
        assert!(target.is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_rejects_symlink_loop() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-symlink-loop");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let a = dir.join("a.md");
        let b = dir.join("b.md");
        // 相互に参照する symlink ループ（a → b → a）を作る。
        symlink(&b, &a).expect("failed to create symlink a");
        symlink(&a, &b).expect("failed to create symlink b");

        // ループは反復回数の上限で打ち切られ、無限ループにならずエラーになる。
        let result = atomic_write(&a, b"should not be written");
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_relative_symlink() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-rel-symlink");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let target = dir.join("real.md");
        let link = dir.join("link.md");
        std::fs::write(&target, b"old").expect("failed to write target");
        // リンクと同じディレクトリの実体を指す相対パスの symlink。
        symlink("real.md", &link).expect("failed to create relative symlink");

        atomic_write(&link, b"new content").expect("atomic_write should follow relative symlink");

        // リンク自体は保持され、相対リンク先の実体が更新される。
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_multi_level_symlink_chain() {
        use std::os::unix::fs::symlink;

        let dir = make_temp_dir("get-md-atomic-symlink-chain");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let real = dir.join("real.md");
        let mid = dir.join("mid.md");
        let entry = dir.join("entry.md");
        std::fs::write(&real, b"old").expect("failed to write target");
        // entry → mid → real の 2 段 symlink チェーン。
        symlink(&real, &mid).expect("failed to create mid symlink");
        symlink(&mid, &entry).expect("failed to create entry symlink");

        atomic_write(&entry, b"new content").expect("atomic_write should resolve the chain");

        // 入口リンクは保持され、チェーン終端の実体が更新される。
        assert!(
            std::fs::symlink_metadata(&entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&real).unwrap(), b"new content");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_file_guard_removes_file_when_not_persisted() {
        // persisted=false で Drop されたら一時ファイルは削除されること。
        let dir = make_temp_dir("get-md-guard-cleanup");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("orphan.tmp");
        std::fs::write(&path, b"junk").expect("failed to write fixture");
        assert!(path.exists());
        {
            let _guard = TempFileGuard {
                path: path.clone(),
                persisted: false,
            };
        } // Drop here
        assert!(!path.exists(), "persisted=false の Drop で削除されるべき");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_file_guard_keeps_file_when_persisted() {
        // persisted=true（rename 成功後）は Drop で削除しないこと。
        let dir = make_temp_dir("get-md-guard-keep");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("kept.tmp");
        std::fs::write(&path, b"data").expect("failed to write fixture");
        {
            let _guard = TempFileGuard {
                path: path.clone(),
                persisted: true,
            };
        }
        assert!(
            path.exists(),
            "persisted=true なら Drop で削除されてはならない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_temp_file_returns_unique_paths_across_calls() {
        // 連続呼び出しで衝突しないことを直接検証する。
        let dir = make_temp_dir("get-md-create-temp-unique");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let (p1, f1) = create_temp_file(&dir).expect("first temp file");
        let (p2, f2) = create_temp_file(&dir).expect("second temp file");
        assert_ne!(p1, p2, "二度呼び出しで同じ一時ファイル名が返ってはいけない");
        assert!(p1.exists());
        assert!(p2.exists());
        drop(f1);
        drop(f2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_temp_file_creates_inside_specified_parent() {
        // 引数で指定した親ディレクトリの直下に作成されること。
        let dir = make_temp_dir("get-md-create-temp-parent");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let (path, file) = create_temp_file(&dir).expect("temp file");
        assert_eq!(path.parent().unwrap(), dir.as_path());
        // Windows でクリーンアップ前にハンドルを閉じる。
        drop(file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_output_write_path_returns_regular_file_unchanged() {
        // 通常ファイルはそのままのパスを返すこと。
        let dir = make_temp_dir("get-md-resolve-regular");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let file = dir.join("plain.md");
        std::fs::write(&file, b"data").expect("failed to write fixture");
        let resolved = resolve_output_write_path(&file).expect("resolve regular file");
        assert_eq!(resolved, file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_output_write_path_returns_missing_path_unchanged() {
        // 存在しないパスもそのまま返すこと（新規作成パスの想定）。
        let dir = make_temp_dir("get-md-resolve-missing");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let missing = dir.join("does-not-exist.md");
        let resolved = resolve_output_write_path(&missing).expect("resolve missing path");
        assert_eq!(resolved, missing);
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

    #[test]
    fn resolve_four_space_indented_backticks_do_not_open_fence() {
        // 4 スペースインデントのバッククォート行をフェンス扱いすると、
        // 後続の通常リンクがフェンス内として解決されなくなる。
        let input = "    ```\n[link](./next.md)";
        let expected = "    ```\n[link](https://example.com/docs/en/next.md)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
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

    #[test]
    fn compact_four_space_indented_backticks_do_not_open_fence() {
        // CommonMark では 4 スペースインデントのバッククォート行はフェンスではない。
        // ここをフェンス扱いすると後続の通常テーブルまで圧縮されなくなる。
        let input = "    ```\n| padded           | table           |";
        let expected = "    ```\n| padded | table |";
        assert_eq!(compact_markdown(input), expected);
    }

    #[test]
    fn compact_four_space_indented_backticks_do_not_close_fence() {
        // 閉じフェンスも最大 3 スペースまで。4 スペース行で閉じるとフェンス内の
        // テーブル行を通常テーブルとして誤って圧縮してしまう。
        let input = "```\n| keep           | spacing        |\n    ```\n| still          | code           |\n```";
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

    #[test]
    fn inline_code_closer_stops_at_fence_start() {
        // CommonMark ではインラインコードはフェンスコードブロック境界を越えない。
        // 未閉鎖の `` ` `` の後にフェンス開始行が現れたら、その先のフェンス内 `` ` `` を
        // 閉じ列として誤認してはいけない。
        let md = "alpha\n```\n`\n```";
        // start=0 から長さ1で閉じを探すと、フェンス開始行で打ち切られて false
        assert!(!has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_stops_at_fence_start_in_blockquote() {
        // ブロッククォート内のフェンス開始も境界として扱う
        let md = "alpha\n> ```\n`\n> ```";
        assert!(!has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_finds_closer_before_fence() {
        // フェンス開始より前に閉じ列があれば true
        let md = "alpha`\n```\n`\n```";
        // start=0, tick_len=1: 行内に閉じ ` があるため true
        assert!(has_matching_inline_code_closer(md, 0, 1));
    }

    #[test]
    fn inline_code_closer_stops_at_blank_line() {
        // CommonMark ではインラインコードは段落(空行)を越えない。
        // 空行の先にあるバッククォートを閉じ列として誤認してはいけない。
        assert!(!has_matching_inline_code_closer("a\n\nb `", 0, 1));
        // 空白/タブだけの行も blank line として扱う。
        assert!(!has_matching_inline_code_closer("a\n \t\nb `", 0, 1));
        // CRLF の空行も同様に打ち切る。
        assert!(!has_matching_inline_code_closer("a\r\n\r\nb `", 0, 1));
        // ブロッククォート記号だけの行もクォート内の空行として扱う。
        assert!(!has_matching_inline_code_closer("a\n>\nb `", 0, 1));
    }

    #[test]
    fn inline_code_closer_finds_closer_before_blank_line() {
        // 空行より前に閉じ列があれば従来どおり true
        assert!(has_matching_inline_code_closer("a`\n\nb", 0, 1));
    }

    #[test]
    fn inline_code_closer_ignores_indented_fence_lookalike() {
        // 4 スペース以上インデントされた「フェンスもどき」はインデントコード扱いで
        // フェンスではないため、`has_matching_inline_code_closer` は閉じを探し続ける。
        let md = "alpha\n    ```\n`";
        assert!(has_matching_inline_code_closer(md, 0, 1));
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
        // 深さ: 1→2→3→2→1→0（インデックス 9）
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
        // フォームフィードとバックスペースは \uXXXX にエスケープする
        let result = escape_js_string("a\x08b\x0cc");
        assert_eq!(result, r#""a\u0008b\u000cc""#);
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
        // 行の途中から開始した場合、``` はインラインコード開始候補になる。
        // CommonMark ではインラインコードはフェンス境界を越えないため、未閉鎖の `` ``` `` は
        // リテラル扱いとなり、後続行の [link](url) は通常リンクとして検出される。
        let md = "text ```\n[link](url)\n```";
        let result = find_next_link_candidate(md, 5);
        assert!(result.is_some());
        let pos = result.unwrap();
        assert!(md[pos..].starts_with("](url)"));
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
        // バグ再現: 書き込みでファイルを作成した後でも
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

        // 書き込みでファイルを作成する（書き込み後はファイルが存在する点が実プログラムと同じ）
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
        // 深さ 2 では引用符はタイトルにならないため、中の ')' は通常のネスト閉じ
        let input = r#"a("b)c)"#;
        // 深さ 1: 'a', '(' で深さ 2、'"' は深さ 2 以上なのでタイトルにならない
        // 'b', ')' で深さ 1、'c', ')' で深さ 0
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

    // --- ネストしたリスト項目内のリンク解決（インデントコード誤判定の回帰） ---

    #[test]
    fn resolve_link_in_nested_unordered_list() {
        // 3 段ネストの箇条書きは 4 スペースインデントになるが、CommonMark では
        // リスト項目の内容でありインデントコードではない。
        let input = "* a\n  * b\n    * c [deep](./deep.html)";
        let expected = "* a\n  * b\n    * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_nested_dash_and_plus_list() {
        // `-` と `+` のマーカーも同様に扱う
        let input = "- a\n  + b\n    - c [deep](./deep.html)";
        let expected = "- a\n  + b\n    - c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_nested_ordered_list() {
        // 順序付きリストは 1 段あたり 3 スペースなので 3 段目で 6 スペースになる
        let input = "1. a\n   1. b\n      1. c [deep](./deep.html)";
        let expected = "1. a\n   1. b\n      1. c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_nested_ordered_list_with_paren_marker() {
        // `1)` 形式の順序付きマーカーも認識する
        let input = "1) a\n   1) b\n      1) c [deep](./deep.html)";
        let expected = "1) a\n   1) b\n      1) c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_mixed_nested_list() {
        // 箇条書きと順序付きが混在するネストでも内容インデントを追跡する
        let input = "* a\n  1. b\n     * c [deep](./deep.html)";
        let expected = "* a\n  1. b\n     * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_wide_ordered_marker_list() {
        // 桁数の多い番号ではマーカー幅も広がる
        let input = "100. a\n     200. b\n          300. c [deep](./deep.html)";
        let expected =
            "100. a\n     200. b\n          300. c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_list_item_with_four_spaces_after_marker() {
        // マーカー直後の空白 4 個までは内容インデントに算入する
        let input = "*    a\n     [next](./next.html)";
        let expected = "*    a\n     [next](https://example.com/docs/en/next.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_empty_list_item_hierarchy() {
        // マーカーだけの空リスト項目でも内容インデントは「マーカー + 空白 1 個」
        let input = "*\n  * b\n    * c [deep](./deep.html)";
        let expected = "*\n  * b\n    * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_loose_nested_list() {
        // loose list は項目の間に空行が入るが、空行でリストは閉じない
        let input = "* a\n\n  * b\n\n    * c [deep](./deep.html)";
        let expected = "* a\n\n  * b\n\n    * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_list_item_continuation_paragraph() {
        // リスト項目の 2 段落目（マーカーの無い継続行）もリスト内容として扱う
        let input = "* a\n  * b\n    * c\n\n      second [p2](./p2.html)";
        let expected =
            "* a\n  * b\n    * c\n\n      second [p2](https://example.com/docs/en/p2.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_after_dedent_to_sibling_list_item() {
        // 深い項目から浅い兄弟項目へ戻ったら、内側の内容インデントは破棄する
        let input = "* a\n  * b\n    * c\n  * d [x](./x.html)\n    text [z](./z.html)";
        let expected = "* a\n  * b\n    * c\n  * d [x](https://example.com/docs/en/x.html)\n    text [z](https://example.com/docs/en/z.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_blockquote_nested_list() {
        // ブロッククォート内のネストしたリストも同様に解決する
        let input = "> * a\n>   * b\n>     * c [deep](./deep.html)";
        let expected = "> * a\n>   * b\n>     * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_multi_level_blockquote_nested_list() {
        // 多段ブロッククォート内のネストしたリスト
        let input = ">> * a\n>>   * b\n>>     * c [deep](./deep.html)";
        let expected = ">> * a\n>>   * b\n>>     * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_blockquote_loose_nested_list() {
        // ブロッククォート記号だけの行（クォート内の空行）を挟む loose list
        let input = "> * a\n>\n>   * b\n>\n>     * c [deep](./deep.html)";
        let expected =
            "> * a\n>\n>   * b\n>\n>     * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_in_table_inside_nested_list() {
        // ネストしたリスト内のテーブル行に含まれるリンクも解決する
        let input = "* a\n  * b\n    * | [x](./x.html) |\n      | - |";
        let expected = "* a\n  * b\n    * | [x](https://example.com/docs/en/x.html) |\n      | - |";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_multiple_links_on_nested_list_line() {
        // 同一のリスト行に複数のリンクがある場合
        let input = "* a\n  * b\n    * c [x](./x.html) and [z](./z.html)";
        let expected = "* a\n  * b\n    * c [x](https://example.com/docs/en/x.html) and [z](https://example.com/docs/en/z.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_nested_list_link_after_multiline_link() {
        // 改行をまたぐリンクでカーソルが飛んだ後もリストの入れ子状態を失わない
        let input = "[first](\n./first\n)\n* a\n  * b\n    * c [deep](./deep.html)";
        // リンク先の前後の空白は URL 解決時に取り除かれる（既存の挙動）
        let expected = "[first](https://example.com/docs/en/first\n)\n* a\n  * b\n    * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_nested_list_link_after_broken_link_candidate() {
        // 閉じ `)` の無い壊れたリンク候補の後でも、リスト内リンクは解決する
        let input = "[broken](\n\n* a\n  * b\n    * c [deep](./deep.html)";
        let expected =
            "[broken](\n\n* a\n  * b\n    * c [deep](https://example.com/docs/en/deep.html)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- リスト内でも真のインデント/フェンスコードは URL 解決しない ---

    #[test]
    fn resolve_link_inside_list_item_indented_code_unchanged() {
        // `* ` の内容インデントは 2 なので、6 スペースは項目内のインデントコード
        let input = "* item\n\n      [skip](./code)\n\n  [real](./page)";
        let expected =
            "* item\n\n      [skip](./code)\n\n  [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_ordered_list_item_indented_code_unchanged() {
        // `1. ` の内容インデントは 3 なので、7 スペースは項目内のインデントコード
        let input = "1. item\n\n       [skip](./code)\n\n   [real](./page)";
        let expected =
            "1. item\n\n       [skip](./code)\n\n   [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_wide_marker_gap_indented_code_unchanged() {
        // マーカー直後の空白が 5 個以上のときは「マーカー + 空白 1 個」が内容インデント
        let input = "*     a\n      [skip](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_in_top_level_indented_code_after_list_unchanged() {
        // リストが終わった後のトップレベル 4 スペースはインデントコードに戻る
        let input = "* a\n\nplain\n\n    [skip](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_inside_deep_list_fence_unchanged() {
        // リスト内のフェンスはリスト内容インデント基準で認識する
        let input = "* a\n  * b\n    ```rust\n    [skip](./code)\n    ```\n    [real](./page)";
        let expected = "* a\n  * b\n    ```rust\n    [skip](./code)\n    ```\n    [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_deep_list_tilde_fence_unchanged() {
        // チルダフェンスもリスト内容インデント基準で認識する
        let input = "* a\n  * b\n    ~~~\n    [skip](./code)\n    ~~~\n    [real](./page)";
        let expected = "* a\n  * b\n    ~~~\n    [skip](./code)\n    ~~~\n    [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_blockquote_list_fence_unchanged() {
        // ブロッククォート内のリストにあるフェンスも同様
        let input = "> * a\n>   ```\n>   [skip](./code)\n>   ```\n>   [real](./page)";
        let expected = "> * a\n>   ```\n>   [skip](./code)\n>   ```\n>   [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn list_like_line_inside_fence_does_not_change_list_state() {
        // フェンス内のリスト風の行で内容インデントを積んではいけない。
        // 積んでしまうと、フェンス後のインデントコードが通常行と誤判定される。
        let input = "* a\n  ```\n  * fake\n  ```\n      [skip](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn resolve_link_after_list_fence_container_ends() {
        // リスト内で開いたフェンスは、そのリスト項目より浅い行でコンテナごと閉じる。
        // 閉じないままだと後続のトップレベル行がコード扱いになり URL 解決されない。
        let input = "* a\n    ```\n[real](./page)";
        let expected = "* a\n    ```\n[real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn closing_fence_indent_is_relative_to_container_not_opening_line() {
        // 閉じフェンスの追加インデントはコンテナ基準で最大 3 スペース。
        // 開始行のインデント基準にすると 6 スペースの行を閉じフェンスと誤認する。
        let input = "   ```\n      ```\n[inside](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn thematic_break_is_not_treated_as_list_item() {
        // `* * *` はテーマ区切りなのでリストの内容インデントを積まない。
        // 積んでしまうと後続の 4 スペース行がインデントコードと判定されなくなる。
        let input = "* * *\n\n    [skip](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
        let input = "- - -\n\n    [skip](./code)";
        assert_eq!(resolve_markdown_urls(input, BASE), input);
    }

    #[test]
    fn is_thematic_break_boundaries() {
        assert!(is_thematic_break("***"));
        assert!(is_thematic_break("* * *"));
        assert!(is_thematic_break("---"));
        assert!(is_thematic_break("- - -"));
        assert!(is_thematic_break("___"));
        assert!(is_thematic_break("_ _ _ _"));
        // マーカーが 3 個未満、文字が混在、他の文字を含む場合はテーマ区切りではない
        assert!(!is_thematic_break("**"));
        assert!(!is_thematic_break("*-*"));
        assert!(!is_thematic_break("* item"));
        assert!(!is_thematic_break("1. a"));
        assert!(!is_thematic_break(""));
    }

    #[test]
    fn blank_line_inside_list_fence_stays_code() {
        // フェンス内の空行はコード扱いのままで、フェンスも閉じない
        let input = "* a\n  ```\n\n  [skip](./code)\n  ```\n  [real](./page)";
        let expected =
            "* a\n  ```\n\n  [skip](./code)\n  ```\n  [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn blockquote_container_end_closes_unclosed_fence() {
        // ブロッククォートが終われば、その中で開いた未閉鎖フェンスも終了する。
        // 状態を引き継ぐと後続のトップレベルリンクまでコード扱いになる。
        let input = "> ```\n> [skip](./code)\n[real](./page)";
        let expected = "> ```\n> [skip](./code)\n[real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn fence_on_list_marker_line_stays_code() {
        // htmd が出力する `* ``` ` 形式でも、コード内リンクは書き換えず、
        // リスト項目より浅い後続行ではフェンスを終了する。
        let input = "* ```\n  [skip](./code)\n[real](./page)";
        let expected = "* ```\n  [skip](./code)\n[real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);

        // 順序付きリストではマーカー幅が異なり、閉じフェンス後も項目が続く。
        let input = "1. ```rust\n   [skip](./code)\n   ```\n   [real](./page)";
        let expected =
            "1. ```rust\n   [skip](./code)\n   ```\n   [real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- リストマーカー判定のヘルパー直接テスト ---

    #[test]
    fn list_marker_len_bullet_markers() {
        assert_eq!(list_marker_len("- a"), Some(1));
        assert_eq!(list_marker_len("* a"), Some(1));
        assert_eq!(list_marker_len("+ a"), Some(1));
    }

    #[test]
    fn list_marker_len_ordered_markers() {
        assert_eq!(list_marker_len("1. a"), Some(2));
        assert_eq!(list_marker_len("12) a"), Some(3));
        assert_eq!(list_marker_len("123456789. a"), Some(10));
    }

    #[test]
    fn list_marker_len_rejects_invalid_markers() {
        // 10 桁以上の番号は CommonMark のリストマーカーではない
        assert_eq!(list_marker_len("1234567890. a"), None);
        // 区切り記号が無い / 数字でも英字でもない
        assert_eq!(list_marker_len("1 a"), None);
        assert_eq!(list_marker_len("1"), None);
        assert_eq!(list_marker_len("a. x"), None);
        assert_eq!(list_marker_len(""), None);
    }

    #[test]
    fn list_item_content_indent_basic_cases() {
        assert_eq!(list_item_content_indent(0, "* a"), Some(2));
        assert_eq!(list_item_content_indent(2, "- b"), Some(4));
        assert_eq!(list_item_content_indent(0, "1. a"), Some(3));
    }

    #[test]
    fn list_item_content_indent_marker_without_space_is_not_a_list() {
        assert_eq!(list_item_content_indent(0, "*foo"), None);
        assert_eq!(list_item_content_indent(0, "1.foo"), None);
    }

    #[test]
    fn list_item_content_indent_wide_and_empty_markers() {
        // 空白 4 個までは内容インデントに算入する
        assert_eq!(list_item_content_indent(0, "*    a"), Some(5));
        // 空白 5 個以上は「マーカー + 空白 1 個」
        assert_eq!(list_item_content_indent(0, "*     a"), Some(2));
        // マーカーのみ / 空白のみ / タブ区切りも「マーカー + 空白 1 個」
        assert_eq!(list_item_content_indent(0, "*"), Some(2));
        assert_eq!(list_item_content_indent(0, "*   "), Some(2));
        assert_eq!(list_item_content_indent(0, "*\ta"), Some(2));
    }

    #[test]
    fn split_leading_spaces_counts_spaces() {
        assert_eq!(split_leading_spaces("abc"), Some((0, "abc")));
        assert_eq!(split_leading_spaces("  ab"), Some((2, "ab")));
        assert_eq!(split_leading_spaces("   "), Some((3, "")));
        assert_eq!(split_leading_spaces(""), Some((0, "")));
    }

    #[test]
    fn split_leading_spaces_rejects_tabs() {
        // タブはインデント幅の計算が必要なので None（安全側でコード扱い）
        assert_eq!(split_leading_spaces("\tab"), None);
        assert_eq!(split_leading_spaces(" \tab"), None);
    }

    #[test]
    fn strip_blockquote_prefix_for_scan_levels() {
        assert_eq!(strip_blockquote_prefix_for_scan("a"), ("a", 0));
        assert_eq!(strip_blockquote_prefix_for_scan("> a"), ("a", 1));
        assert_eq!(strip_blockquote_prefix_for_scan(">a"), ("a", 1));
        assert_eq!(strip_blockquote_prefix_for_scan(">> a"), ("a", 2));
        assert_eq!(strip_blockquote_prefix_for_scan("   > a"), ("a", 1));
        assert_eq!(strip_blockquote_prefix_for_scan(">"), ("", 1));
        // 記号の直後に残った空白はインデントとして保持する
        assert_eq!(strip_blockquote_prefix_for_scan(">     a"), ("    a", 1));
        // 4 スペース以上インデントされた `>` はブロッククォートではない
        assert_eq!(strip_blockquote_prefix_for_scan("    > a"), ("    > a", 0));
    }

    // --- MarkdownBlockMap の行分類 ---

    #[test]
    fn block_map_classifies_blank_code_and_normal_lines() {
        let md = "text\n\n    code\n* a\n";
        assert_eq!(line_kind_at(md, 0), Some(LinkScanLineKind::Normal));
        assert_eq!(line_kind_at(md, 5), Some(LinkScanLineKind::Blank));
        assert_eq!(line_kind_at(md, 6), Some(LinkScanLineKind::Code));
        assert_eq!(line_kind_at(md, 15), Some(LinkScanLineKind::Normal));
    }

    #[test]
    fn block_map_returns_none_for_non_line_start_offset() {
        // 行頭以外のオフセットでは分類を返さない
        assert_eq!(line_kind_at("text\nmore", 2), None);
    }

    #[test]
    fn block_map_marks_tab_indented_line_as_code() {
        // タブインデントは安全側でコード扱い
        assert_eq!(line_kind_at("\t[x](./y)", 0), Some(LinkScanLineKind::Code));
    }

    #[test]
    fn resolve_link_inside_indented_code_unchanged() {
        // CommonMark のインデントコードブロック内は URL 解決対象から除外する
        let input = "    [skip](./code)\n[real](./page)";
        let expected = "    [skip](./code)\n[real](https://example.com/docs/en/page)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_inside_blockquote_indented_code_unchanged() {
        // ブロッククォート内のインデントコードブロックも URL 解決対象から除外する
        let input = ">     [skip](./code)\n> [real](./page)";
        let expected = ">     [skip](./code)\n> [real](https://example.com/docs/en/page)";
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

    #[test]
    fn resolve_broken_nested_link_keeps_outer_link_resolution() {
        // 外側リンクのテキスト内に閉じ括弧のないリンク候補があっても、
        // 未閉鎖の外側 `[` を引き継ぎ、内側リンクと外側リンクの両方を解決する。
        let input = "[outer [broken](./missing [inner](./ok)](./target)";
        let expected = "[outer [broken](./missing [inner](https://example.com/docs/en/ok)](https://example.com/docs/en/target)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_broken_nested_image_link_keeps_outer_link_resolution() {
        // 画像リンク形式でも、壊れた候補の後に残る外側リンクの開き括弧を維持する。
        let input = "[![broken](./missing ![inner](./img.png)](./outer)";
        let expected = "[![broken](./missing ![inner](https://example.com/docs/en/img.png)](https://example.com/docs/en/outer)";
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
        assert_eq!(fence_marker_after_blockquote("   > ```"), Some(('`', 3)));
        assert_eq!(fence_marker_after_blockquote("  > ```"), Some(('`', 3)));
    }

    #[test]
    fn fence_marker_after_blockquote_rejects_four_space_indent() {
        // ブロッククォート前でも 4 スペースはインデントコード扱いで、フェンスではない。
        assert_eq!(fence_marker_after_blockquote("    > ```"), None);
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
        let (pos, count) = super::find_next_link_candidate(md, 3, 1, &MarkdownBlockMap::build(md));
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
        let (pos, count) = super::find_next_link_candidate(md, 4, 1, &MarkdownBlockMap::build(md));
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
        let (pos, count) = super::find_next_link_candidate(md, 1, 0, &MarkdownBlockMap::build(md));
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
        let (pos, count) = super::find_next_link_candidate(md, 1, 0, &MarkdownBlockMap::build(md));
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
        let (pos, count) =
            super::find_next_link_candidate(md, start, 0, &MarkdownBlockMap::build(md));
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

    // --- is_closing_fence_line_after_indent の直接テスト ---

    #[test]
    fn closing_fence_after_indent_no_indent() {
        // インデントなしのマーカーのみは閉じフェンスとして妥当
        assert!(is_closing_fence_line_after_indent("```", '`', 3));
        assert!(is_closing_fence_line_after_indent("~~~", '~', 3));
    }

    #[test]
    fn closing_fence_after_indent_allows_up_to_three_spaces() {
        // 最大 3 スペースのインデントは閉じフェンスとして許容する
        assert!(is_closing_fence_line_after_indent(" ```", '`', 3));
        assert!(is_closing_fence_line_after_indent("   ```", '`', 3));
    }

    #[test]
    fn closing_fence_after_indent_rejects_four_space_or_tab_indent() {
        // 4 スペース以上・タブはインデントコードブロックなので閉じフェンスにしない
        assert!(!is_closing_fence_line_after_indent("    ```", '`', 3));
        assert!(!is_closing_fence_line_after_indent("\t```", '`', 3));
    }

    #[test]
    fn closing_fence_after_indent_rejects_info_string() {
        // インデントを剥がした後も info string 付きは閉じフェンスにしない
        assert!(!is_closing_fence_line_after_indent("  ```rust", '`', 3));
    }

    #[test]
    fn closing_fence_after_indent_allows_trailing_cr() {
        // 末尾 CR は line ending として無視し CRLF 入力でも閉じる
        assert!(is_closing_fence_line_after_indent("  ```\r", '`', 3));
    }

    #[test]
    fn closing_fence_after_indent_respects_min_len() {
        // 開始フェンスより短いマーカーは閉じず、同長以上なら閉じる
        assert!(!is_closing_fence_line_after_indent("  ```", '`', 4));
        assert!(is_closing_fence_line_after_indent("  ````", '`', 4));
    }

    // --- ブロッククォート内の閉じフェンス判定（MarkdownBlockMap 経由） ---

    /// ブロッククォート内のフェンスが `close_line` で閉じるかを、
    /// その次の行が通常行（= リンク走査対象）に戻るかどうかで確かめる。
    fn blockquote_fence_closes(open_line: &str, close_line: &str) -> bool {
        let marker_start = open_line
            .char_indices()
            .find_map(|(index, ch)| matches!(ch, '`' | '~').then_some(index))
            .expect("opening fence marker must exist");
        let prefix = &open_line[..marker_start];
        let md = format!("{open_line}\n{prefix}body\n{close_line}\n{prefix}after");
        let after_offset = md.len() - prefix.len() - "after".len();
        line_kind_at(&md, after_offset) == Some(LinkScanLineKind::Normal)
    }

    #[test]
    fn closing_fence_after_blockquote_with_marker_only() {
        // ブロッククォート記号 + マーカーのみは閉じフェンスとして妥当
        assert!(blockquote_fence_closes("> ```", "> ```"));
        assert!(blockquote_fence_closes(">> ~~~", ">> ~~~"));
    }

    #[test]
    fn closing_fence_after_blockquote_rejects_info_string() {
        // ブロッククォート内でも info string 付きは閉じフェンスにしない
        assert!(!blockquote_fence_closes("> ```", "> ```rust"));
        assert!(!blockquote_fence_closes(">> ```", ">> ```py"));
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

    // --- strip_fence_blockquote_markers の直接テスト ---

    #[test]
    fn strip_fence_blockquote_markers_no_marker() {
        // ブロッククォート記号がない行はそのまま返す
        assert_eq!(
            strip_fence_blockquote_markers("plain text"),
            Some("plain text")
        );
    }

    #[test]
    fn strip_fence_blockquote_markers_single_level() {
        // `>` + スペース、および `>` 直後にスペースが無い場合も取り除く
        assert_eq!(strip_fence_blockquote_markers("> content"), Some("content"));
        assert_eq!(strip_fence_blockquote_markers(">content"), Some("content"));
    }

    #[test]
    fn strip_fence_blockquote_markers_nested_levels() {
        // 多段ブロッククォート記号を順に取り除く
        assert_eq!(strip_fence_blockquote_markers(">> deep"), Some("deep"));
    }

    #[test]
    fn strip_fence_blockquote_markers_allows_up_to_three_leading_spaces() {
        // 行頭は最大 3 スペースまで許容してから記号を処理する
        assert_eq!(
            strip_fence_blockquote_markers("   > content"),
            Some("content")
        );
    }

    #[test]
    fn strip_fence_blockquote_markers_rejects_four_leading_spaces() {
        // strip_blockquote_markers と異なり、4 スペース以上の行頭インデントは
        // CommonMark のインデントコードブロック扱いで None を返す
        assert_eq!(strip_fence_blockquote_markers("    > content"), None);
    }

    #[test]
    fn strip_fence_blockquote_markers_allows_up_to_three_spaces_after_marker() {
        // `>` の後は 1 スペース + 最大 3 スペースインデントまで許容する
        assert_eq!(
            strip_fence_blockquote_markers(">    content"),
            Some("content")
        );
    }

    #[test]
    fn strip_fence_blockquote_markers_rejects_excess_spaces_after_marker() {
        // `>` の後が過剰インデント(1 スペース + 4 スペース)になると None
        assert_eq!(strip_fence_blockquote_markers(">     content"), None);
    }

    #[test]
    fn strip_fence_blockquote_markers_markers_only() {
        // ブロッククォート記号だけの行は空文字列を返す
        assert_eq!(strip_fence_blockquote_markers(">>"), Some(""));
    }

    #[test]
    fn strip_fence_blockquote_markers_empty_line() {
        // 空行はそのまま空文字列を返す
        assert_eq!(strip_fence_blockquote_markers(""), Some(""));
    }

    // --- ブロッククォート内の閉じフェンス判定: 追加境界テスト ---

    #[test]
    fn closing_fence_after_blockquote_trailing_spaces_allowed() {
        // ブロッククォート内の閉じフェンス + 末尾空白
        assert!(blockquote_fence_closes("> ```", "> ```   "));
    }

    #[test]
    fn closing_fence_after_blockquote_trailing_cr_allowed() {
        // ブロッククォート内でも末尾 CR は line ending として無視
        assert!(blockquote_fence_closes("> ```", "> ```\r"));
    }

    #[test]
    fn closing_fence_after_blockquote_longer_marker() {
        // 開始フェンスより長い閉じマーカーも有効
        assert!(blockquote_fence_closes(">> ```", ">> `````"));
    }

    #[test]
    fn closing_fence_after_blockquote_shorter_marker_rejected() {
        // 開始フェンスより短いマーカーは閉じ扱いしない
        assert!(!blockquote_fence_closes("> `````", "> ```"));
    }

    #[test]
    fn closing_fence_after_blockquote_different_marker_rejected() {
        // 異なるマーカー文字は閉じ扱いしない
        assert!(!blockquote_fence_closes("> ```", "> ~~~"));
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

    #[test]
    fn compact_consecutive_separator_rows_second_not_normalized() {
        // セパレータ行が連続する場合、1 本目はセパレータとして正規化されるが、
        // 2 本目は table_state が Body のため正規化対象外となり、ダッシュがそのまま残る。
        let input = "| --- |\n| --- |";
        assert_eq!(compact_markdown(input), "| - |\n| --- |");
    }

    #[test]
    fn split_link_destination_leading_quote_without_whitespace_is_url() {
        // 先頭に空白が無くクォートで始まるリンク先は「空のリンク先 + title」ガード
        // (body.len() < inside.len()) に該当せず、標準形式として全体が URL 扱いになる。
        assert_eq!(
            split_link_destination("\"title\""),
            ("\"title\"", "", false)
        );
    }

    #[test]
    fn find_close_paren_quote_after_space_then_nonquote_is_not_title() {
        // 空白 (saw_sep_ws=true) の直後に非クォート文字が来ると saw_sep_ws がリセットされ、
        // 続くクォートは title 開始として扱われない。よって後続の `)` が終端になる。
        assert_eq!(find_link_close_paren("a b\"c)"), Some(5));
    }

    #[test]
    fn link_candidate_multiline_inline_code_then_link() {
        // インラインコードが改行をまたぐ場合でも、閉じバッククォートまで正しくスキップし、
        // その後ろのリンクの `](` 位置を返す。
        let md = "`a\nb` [x](y)";
        assert_eq!(find_next_link_candidate(md, 0), Some(8));
    }

    // --- 改行をまたぐインラインコードが行途中で閉じた後の行頭判定の陳腐化防止 ---

    #[test]
    fn resolve_link_after_multiline_inline_code_closing_before_fence_lookalike() {
        // 改行をまたぐインラインコードが行の途中で閉じた直後に ``` が続いても、
        // 行の残りを「行頭のフェンス開始」と誤認せず、後続行のリンクを解決する。
        // (CommonMark ではフェンスは物理行頭でのみ開始する)
        let input = "`a\nb` ```\n[x](./y)";
        let expected = "`a\nb` ```\n[x](https://example.com/docs/en/y)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn resolve_link_after_multiline_inline_code_closing_before_indented_lookalike() {
        // 改行をまたぐインラインコードが行の途中で閉じた直後に 4 スペースが続いても、
        // 行の残りを「インデントコード行」と誤認せず、同一行のリンクを解決する。
        let input = "`a\n    b`    [x](./y)";
        let expected = "`a\n    b`    [x](https://example.com/docs/en/y)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    #[test]
    fn link_candidate_multiline_inline_code_close_at_line_start_then_fence_lookalike() {
        // 閉じバッククォート列が物理行頭にある場合も、消費後は行頭扱いにしない。
        // 直後の ``` は未閉鎖のリテラルなバッククォート列であり、フェンスではない。
        let input = "`a\n` ```\n[x](./y)";
        let expected = "`a\n` ```\n[x](https://example.com/docs/en/y)";
        assert_eq!(resolve_markdown_urls(input, BASE), expected);
    }

    // --- 4 スペース以上インデントされたテーブル風行はコードとして保持される ---

    #[test]
    fn compact_markdown_indented_table_row_kept_as_code() {
        // CommonMark のインデントコードブロック（4 スペース以上）に該当する行は
        // テーブル扱いせず、行をそのまま保持する。`line.trim()` で先頭インデントを
        // 落としてセル間空白も圧縮すると、コードの内容が破壊される。
        let input = "    | a   | b   |";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_markdown_indented_table_row_with_tab_kept_as_code() {
        // タブ始まりの行も CommonMark のインデントコードブロックとして扱い、
        // テーブル圧縮を適用しない。
        let input = "\t| a   | b   |";
        assert_eq!(compact_markdown(input), input);
    }

    #[test]
    fn compact_markdown_three_space_indent_still_compresses_table() {
        // 3 スペースまでのインデントはテーブル行として許容され、
        // 通常通り圧縮される。
        let input = "   | a   | b   |";
        assert_eq!(compact_markdown(input), "| a | b |");
    }

    // --- インラインコード内の `|` はセル区切りにならない ---

    #[test]
    fn split_unescaped_table_cells_pipe_inside_inline_code() {
        // インラインコードスパン `|` `|` `|` をセル区切りとして扱うと
        // コード内容が壊れる。CommonMark/GFM ではコードスパン内はリテラル扱い。
        let cells = split_unescaped_table_cells(" `a | b` | x ");
        assert_eq!(cells, vec![" `a | b` ", " x "]);
    }

    #[test]
    fn split_unescaped_table_cells_double_backtick_code_with_pipe() {
        // `` ``a|b`` `` のような二重バッククォートでもコードスパン扱いとなり、
        // 内側の `|` をセル区切りとして扱わない。
        let cells = split_unescaped_table_cells(" ``a|b`` | x ");
        assert_eq!(cells, vec![" ``a|b`` ", " x "]);
    }

    #[test]
    fn split_unescaped_table_cells_unmatched_backtick_falls_back_to_split() {
        // 閉じバッククォートがなければインラインコード扱いせず、
        // `|` は通常通りセル区切りとして扱う。
        let cells = split_unescaped_table_cells(" `unclosed | x ");
        assert_eq!(cells, vec![" `unclosed ", " x "]);
    }

    #[test]
    fn split_unescaped_table_cells_escaped_backtick_is_not_code_start() {
        // バックスラッシュでエスケープされたバッククォートはコードスパン開始扱いしない。
        // 続く `|` は通常のセル区切りとなる。
        let cells = split_unescaped_table_cells(r" \`a | b\` | x ");
        assert_eq!(cells, vec![r" \`a ", r" b\` ", " x "]);
    }

    #[test]
    fn compact_table_with_pipe_in_inline_code_cell() {
        // テーブル行内のインラインコードに `|` を含む場合、セル数を保ったまま圧縮する。
        let input = "| `a | b` | x |";
        // 2 セル (`code`, `x`) として圧縮される
        assert_eq!(compact_markdown(input), "| `a | b` | x |");
    }
}
