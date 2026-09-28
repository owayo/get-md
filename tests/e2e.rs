use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use url::Url;

fn get_md_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_get-md"))
}

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Failed to get current time")
            .as_nanos();
        let seq = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("get-md-e2e-{}-{unique}-{seq}", std::process::id()));
        fs::create_dir_all(&path).expect("Failed to create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn write_file(path: &Path, contents: &str) {
    fs::write(path, contents).expect("Failed to write test file");
}

fn file_url(path: &Path) -> String {
    Url::from_file_path(path)
        .expect("Failed to convert path to file URL")
        .into()
}

fn static_http_url(status_code: u16, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("Failed to bind test HTTP server");
    let addr = listener
        .local_addr()
        .expect("Failed to get test HTTP server address");

    thread::spawn(move || {
        for stream in listener.incoming().take(8) {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            let reason = if status_code == 404 {
                "Not Found"
            } else {
                "OK"
            };
            let response = format!(
                "HTTP/1.1 {status_code} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    format!("http://{addr}/")
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn fetch_github_raw_readme() {
    let output = get_md_bin()
        .args([
            "https://raw.githubusercontent.com/owayo/get-md/refs/heads/main/README.md",
            "-q",
            "--no-cache",
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.is_empty(), "Output should not be empty");
    assert!(
        stdout.contains("get-md"),
        "Output should contain 'get-md': got:\n{stdout}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn fetch_local_html_and_resolve_relative_urls() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main>
      <p><a href="./guide.html">Guide</a></p>
      <p><img src="./images/logo.png" alt="Logo"></p>
    </main>
  </body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "main".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let guide_url = file_url(&temp_dir.path().join("guide.html"));
    let image_url = file_url(&temp_dir.path().join("images/logo.png"));
    assert!(
        stdout.contains(&format!("[Guide]({guide_url})")),
        "Resolved guide link was not found: {stdout}",
    );
    assert!(
        stdout.contains(&format!("![Logo]({image_url})")),
        "Resolved image link was not found: {stdout}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn wait_longer_than_previous_idle_timeout_keeps_cdp_connected() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("static.html");
    write_file(
        &page,
        "<html><body><main>Static content</main></body></html>",
    );

    // 旧設定のアイドルタイムアウトは timeout 5 秒 + 30 秒だった。
    // 静的ページを 36 秒待つと CDP が切断され、続く HTML 抽出が失敗していた。
    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".into(),
            "main".into(),
            "-t".into(),
            "5".into(),
            "-w".into(),
            "36".into(),
            "-q".into(),
        ])
        .output()
        .expect("get-md を実行できること");

    assert!(
        output.status.success(),
        "待機後の CDP 接続が切れた: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "Static content");
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn nested_list_relative_urls_are_resolved() {
    // htmd はネストしたリストを 1 段 2 スペースで出力するため、3 段目以降は
    // 行頭 4 スペース以上になる。これをインデントコードと誤判定すると、
    // ナビゲーションや目次のリンクが相対 URL のまま残ってしまう。
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main>
      <ul>
        <li><pre><code>[NotALink](./skip.html)</code></pre></li>
        <li>L1
          <ul>
            <li>L2
              <ul>
                <li>L3 <a href="./deep.html">Deep</a></li>
              </ul>
            </li>
          </ul>
        </li>
      </ul>
    </main>
  </body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "main".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let deep_url = file_url(&temp_dir.path().join("deep.html"));
    assert!(
        stdout.contains(&format!("[Deep]({deep_url})")),
        "Nested list link was not resolved: {stdout}",
    );
    // フェンスコードブロック内のリンクは書き換えない
    assert!(
        stdout.contains("[NotALink](./skip.html)"),
        "Link inside a fenced code block must stay unchanged: {stdout}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn base_href_is_used_for_relative_url_resolution() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <head><base href="https://cdn.example/docs/"></head>
  <body>
    <main><a href="guide.html">Guide</a></main>
  </body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "main".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[Guide](https://cdn.example/docs/guide.html)"),
        "document.baseURI was not used for URL resolution: {stdout}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn multiple_selectors_are_joined_with_separator() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <h1>Title</h1>
    <article><p>Body</p></article>
  </body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "h1".to_string(),
            "-s".to_string(),
            "article".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("\n\n---\n\n"),
        "Selectors were not joined with the documented separator: {stdout}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn invalid_css_selector_is_reported_as_error() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body><main>content</main></body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "[".to_string(),
            "-w".to_string(),
            "0".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        !output.status.success(),
        "get-md should reject an invalid CSS selector: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid CSS selector '['"),
        "stderr should identify the invalid selector: {stderr}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn ignore_date_keeps_existing_output_when_only_timestamp_differs() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    let output_path = temp_dir.path().join("output.md");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main><p>Updated: 2026-04-13 10:00</p></main>
  </body>
</html>"#,
    );
    write_file(&output_path, "Updated: 2026-04-12 09:00\n");

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "main".to_string(),
            "-o".to_string(),
            output_path.display().to_string(),
            "--ignore-date".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.stdout.is_empty(),
        "Output file mode should not write to stdout: {}",
        String::from_utf8_lossy(&output.stdout),
    );

    let saved = fs::read_to_string(&output_path).expect("Failed to read output file");
    assert_eq!(saved, "Updated: 2026-04-12 09:00\n");
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn ignore_date_overwrites_output_when_non_date_content_differs() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    let output_path = temp_dir.path().join("output.md");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main><p>Updated: 2026-04-13 10:00</p><p>Status: done</p></main>
  </body>
</html>"#,
    );
    write_file(&output_path, "Updated: 2026-04-12 09:00\nStatus: pending\n");

    let output = get_md_bin()
        .args([
            file_url(&page),
            "-s".to_string(),
            "main".to_string(),
            "-o".to_string(),
            output_path.display().to_string(),
            "--ignore-date".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let saved = fs::read_to_string(&output_path).expect("Failed to read output file");
    assert!(
        saved.contains("Status: done"),
        "--ignore-date must not suppress non-date content changes: {saved}",
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn front_matter_is_kept_when_only_retrieved_at_differs() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    let output_path = temp_dir.path().join("output.md");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <head><title>  E2E "front" matter: test  </title></head>
  <body>
    <main><p>Hello</p></main>
  </body>
</html>"#,
    );

    let run = |source: &str| {
        let output = get_md_bin()
            .args([
                file_url(&page),
                "-s".to_string(),
                "main".to_string(),
                "-o".to_string(),
                output_path.display().to_string(),
                "--front-matter".to_string(),
                "--meta".to_string(),
                format!("source={source}"),
                "--meta".to_string(),
                "note=".to_string(),
                "-w".to_string(),
                "0".to_string(),
                "-q".to_string(),
            ])
            .output()
            .expect("Failed to execute get-md");
        assert!(
            output.status.success(),
            "get-md exited with error: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            output.stdout.is_empty(),
            "Output file mode should not write to stdout: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        fs::read_to_string(&output_path).expect("Failed to read output file")
    };

    // 1 回目: 取得元の情報を front matter にして書く。URL は変わらないので final_url は出さない
    let first = run("e2e");
    let retrieved_at = first
        .lines()
        .find_map(|line| line.strip_prefix("retrieved_at: "))
        .expect("front matter should contain retrieved_at")
        .to_string();
    let rfc3339_seconds = Regex::new(r#"^"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z"$"#).unwrap();
    assert!(
        rfc3339_seconds.is_match(&retrieved_at),
        "retrieved_at should be UTC RFC 3339 with seconds: {retrieved_at}",
    );
    let expected = format!(
        concat!(
            "---\n",
            "url: \"{url}\"\n",
            "title: \"E2E \\\"front\\\" matter: test\"\n",
            "selectors: [\"main\"]\n",
            "retrieved_at: {retrieved_at}\n",
            "source: \"e2e\"\n",
            "note: \"\"\n",
            "---\n",
            "\n",
            "Hello\n",
        ),
        url = file_url(&page),
        retrieved_at = retrieved_at,
    );
    assert_eq!(first, expected);

    // 前に取得したファイルに見立てて、retrieved_at を過去の時刻にしておく
    let previous = first.replace(&retrieved_at, "\"2000-01-01T00:00:00Z\"");
    write_file(&output_path, &previous);

    // 2 回目: 取得した時刻だけが違うので書き換えない (retrieved_at は前の値のまま)
    let second = run("e2e");
    assert_eq!(
        second, previous,
        "Only retrieved_at differs, so the file must be left unchanged",
    );

    // 3 回目: --meta の値が変わったので書き換え、retrieved_at も新しくなる
    let third = run("e2e-updated");
    assert!(
        third.contains("\nsource: \"e2e-updated\"\n"),
        "A changed --meta value must be written: {third}",
    );
    assert!(
        !third.contains("2000-01-01T00:00:00Z"),
        "A rewritten file must carry the new retrieved_at: {third}",
    );
}

/// 見えなくされた要素・テキストと、見えない文字を含むページ。
///
/// `VISIBLE-` で始まる語は画面に見えるので残り、`HIDDEN-` で始まる語は見えないので既定では除かれる。
const HIDDEN_CONTENT_PAGE: &str = r#"<!doctype html>
<html>
  <head>
    <title>Hidden&#x200B; content&#x202E;</title>
    <style>
      .sr-only {
        position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
        overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
      }
      .clip-path-hidden {
        position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%);
      }
      .offscreen-left { position: absolute; left: -10000px; top: 0; width: 200px; }
      .offscreen-top { position: absolute; top: -500px; left: 0; height: 100px; }
      .below { position: absolute; top: 5000px; left: 0; }
      .gradient {
        background: linear-gradient(90deg, #c00, #00c);
        -webkit-background-clip: text; background-clip: text; color: transparent;
      }
    </style>
  </head>
  <body>
    <main>
      <p>VISIBLE-BASE</p>
      <p style="display: none">HIDDEN-DISPLAY-NONE</p>
      <p hidden>HIDDEN-ATTRIBUTE</p>
      <p style="opacity: 0">HIDDEN-OPACITY</p>
      <div style="opacity: 0"><p>HIDDEN-OPACITY-CHILD</p></div>
      <p style="content-visibility: hidden">HIDDEN-CONTENT-VISIBILITY</p>
      <div style="visibility: hidden">HIDDEN-VISIBILITY
        <span style="visibility: visible">VISIBLE-REVEALED-CHILD</span></div>
      <div style="display: contents"><p>VISIBLE-DISPLAY-CONTENTS</p></div>
      <details>
        <summary>VISIBLE-CLOSED-SUMMARY</summary>
        <p>HIDDEN-CLOSED-DETAILS-BODY</p>
        HIDDEN-CLOSED-DETAILS-TEXT
      </details>
      <details open>
        <summary>VISIBLE-OPEN-SUMMARY</summary>
        <p>VISIBLE-OPEN-DETAILS-BODY</p>
      </details>
      <span class="sr-only">HIDDEN-SR-ONLY</span>
      <span class="clip-path-hidden">HIDDEN-CLIP-PATH</span>
      <div class="offscreen-left">HIDDEN-OFFSCREEN-LEFT</div>
      <div class="offscreen-top">HIDDEN-OFFSCREEN-TOP</div>
      <div class="below">VISIBLE-FAR-BELOW</div>
      <p aria-hidden="true">VISIBLE-ARIA-HIDDEN</p>
      <p style="font-size: 0">HIDDEN-FONT-SIZE-ZERO</p>
      <p style="color: transparent">HIDDEN-TRANSPARENT-COLOR</p>
      <p style="color: rgba(0, 0, 0, 0)">HIDDEN-RGBA-ZERO</p>
      <p style="font-size: 0.5px">VISIBLE-TINY-FONT</p>
      <h2 class="gradient">VISIBLE-GRADIENT-TEXT</h2>
      <p style="color: transparent; text-shadow: 0 0 2px #333">VISIBLE-SHADOW-TEXT</p>
      <p>ZERO&#x200B;WIDTH &#x202E;BIDI&#x202C; TAG&#xE0041;&#xE0042;&#xE007F; &#x1F468;&#x200D;&#x1F469;&#x200D;&#x1F467;</p>
    </main>
  </body>
</html>"#;

/// 手元のページに get-md を実行する。`-q` と `-w 0` は常に付ける。
fn run_get_md_on(page: &Path, extra_args: &[&str]) -> std::process::Output {
    let mut args = vec![
        file_url(page),
        "-w".to_string(),
        "0".to_string(),
        "-q".to_string(),
    ];
    args.extend(extra_args.iter().map(|arg| arg.to_string()));
    get_md_bin()
        .args(args)
        .output()
        .expect("Failed to execute get-md")
}

fn assert_success(output: &std::process::Output) -> String {
    assert!(
        output.status.success(),
        "get-md exited with error: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn visible_and_hidden_words(markdown: &str) -> (Vec<&str>, Vec<&str>) {
    let words = Regex::new(r"\b(VISIBLE|HIDDEN)-[A-Z0-9-]+\b").unwrap();
    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    for word in words.find_iter(markdown).map(|m| m.as_str()) {
        if word.starts_with("VISIBLE-") {
            visible.push(word);
        } else {
            hidden.push(word);
        }
    }
    (visible, hidden)
}

/// `HIDDEN_CONTENT_PAGE` のうち、既定の抽出でも残る語。
const EXPECTED_VISIBLE_WORDS: [&str; 11] = [
    "VISIBLE-BASE",
    // visibility: hidden の親の中で visible に戻した子
    "VISIBLE-REVEALED-CHILD",
    "VISIBLE-DISPLAY-CONTENTS",
    "VISIBLE-CLOSED-SUMMARY",
    "VISIBLE-OPEN-SUMMARY",
    "VISIBLE-OPEN-DETAILS-BODY",
    // 下の外 (スクロールすれば見える) は除かない
    "VISIBLE-FAR-BELOW",
    // aria-hidden は画面の表示を変えない
    "VISIBLE-ARIA-HIDDEN",
    // 1px 未満でも 0 でなければ除かない
    "VISIBLE-TINY-FONT",
    // 塗りが透明でも、背景を文字の形に切り抜いたり影を付けたりした文字は見える
    "VISIBLE-GRADIENT-TEXT",
    "VISIBLE-SHADOW-TEXT",
];

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn hidden_elements_and_invisible_characters_are_removed_by_default() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(&page, HIDDEN_CONTENT_PAGE);

    let output = run_get_md_on(&page, &["-s", "main", "--front-matter"]);
    let stdout = assert_success(&output);
    assert!(
        output.stderr.is_empty(),
        "--quiet prints nothing to stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );

    let (visible, hidden) = visible_and_hidden_words(&stdout);
    assert!(
        hidden.is_empty(),
        "hidden content leaked: {hidden:?}\n{stdout}"
    );
    for word in EXPECTED_VISIBLE_WORDS {
        assert!(visible.contains(&word), "{word} is missing:\n{stdout}");
    }

    // 見えない文字を除く (RGI の絵文字の ZWJ の並びは残す)
    assert!(stdout.contains("ZEROWIDTH BIDI TAG "), "{stdout:?}");
    assert!(
        stdout.contains("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"),
        "{stdout:?}"
    );
    for ch in ['\u{200B}', '\u{202E}', '\u{202C}', '\u{E0041}', '\u{E007F}'] {
        assert!(
            !stdout.contains(ch),
            "U+{:04X} remains: {stdout:?}",
            u32::from(ch)
        );
    }
    // front matter の title からも除く
    assert!(
        stdout.contains("\ntitle: \"Hidden content\"\n"),
        "{stdout:?}"
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn keep_hidden_restores_hidden_content_but_still_removes_invisible_characters() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(&page, HIDDEN_CONTENT_PAGE);

    let stdout = assert_success(&run_get_md_on(&page, &["-s", "main", "--keep-hidden"]));

    let (_, hidden) = visible_and_hidden_words(&stdout);
    for word in [
        "HIDDEN-DISPLAY-NONE",
        "HIDDEN-OPACITY",
        "HIDDEN-VISIBILITY",
        "HIDDEN-CLOSED-DETAILS-BODY",
        "HIDDEN-SR-ONLY",
        "HIDDEN-OFFSCREEN-LEFT",
        "HIDDEN-FONT-SIZE-ZERO",
        "HIDDEN-TRANSPARENT-COLOR",
    ] {
        assert!(
            hidden.contains(&word),
            "{word} is missing with --keep-hidden:\n{stdout}"
        );
    }
    // --keep-hidden は見えない文字には効かない
    assert!(stdout.contains("ZEROWIDTH BIDI TAG "), "{stdout:?}");
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn keep_invisible_keeps_invisible_characters_but_still_removes_hidden_content() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(&page, HIDDEN_CONTENT_PAGE);

    let stdout = assert_success(&run_get_md_on(
        &page,
        &["-s", "main", "--keep-invisible", "--front-matter"],
    ));

    let (_, hidden) = visible_and_hidden_words(&stdout);
    assert!(
        hidden.is_empty(),
        "hidden content leaked: {hidden:?}\n{stdout}"
    );
    assert!(
        stdout.contains("ZERO\u{200B}WIDTH \u{202E}BIDI\u{202C} TAG\u{E0041}\u{E0042}\u{E007F}"),
        "{stdout:?}"
    );
    assert!(
        stdout.contains("\ntitle: \"Hidden\u{200B} content\u{202E}\"\n"),
        "{stdout:?}"
    );
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn page_scripts_cannot_disable_hidden_content_removal() {
    // 見えない指示を紛れ込ませたいページは、判定に使う関数を書き換えて判定を偽ろうとする。
    // 抽出はページのスクリプトから切り離した実行環境 (isolated world) で動くので、書き換えは届かない
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main>
      <p>Visible text</p>
      <p style="display: none">HIDDEN-INJECTED-INSTRUCTION</p>
    </main>
    <script>
      const visible = { display: 'block', visibility: 'visible', opacity: '1', position: 'static',
                        getPropertyValue: () => '' };
      window.getComputedStyle = () => visible;
      Element.prototype.getBoundingClientRect = () => ({ left: 10, top: 10, right: 500, bottom: 50, width: 490, height: 40 });
      JSON.stringify = () => '{"matched":1,"fragments":["<main>HIDDEN-SPOOFED-RESULT</main>"]}';
      Array.from = () => [];
    </script>
  </body>
</html>"#,
    );

    let stdout = assert_success(&run_get_md_on(&page, &["-s", "main"]));
    assert_eq!(stdout, "Visible text");
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn selecting_only_hidden_elements_fails_with_keep_hidden_hint() {
    let temp_dir = TempDir::new();
    let page = temp_dir.path().join("page.html");
    write_file(
        &page,
        r#"<!doctype html>
<html>
  <body>
    <main>Visible body</main>
    <section id="secret" style="display: none"><p>Secret text</p></section>
    <div style="opacity: 0"><aside id="faded">Faded text</aside></div>
  </body>
</html>"#,
    );

    // 選んだ要素そのものが見えない (祖先の opacity: 0 を含む) と、その断片を出さない
    let output = run_get_md_on(&page, &["-s", "#secret", "-s", "#faded"]);
    assert!(
        !output.status.success(),
        "get-md should fail when every selected element is hidden: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--keep-hidden"), "{stderr}");
    assert!(stderr.contains("'#secret'"), "{stderr}");

    // 見える要素も選んでいれば、見えない方を警告して続ける
    let output = run_get_md_on(&page, &["-s", "#secret", "-s", "main"]);
    let stdout = assert_success(&output);
    assert_eq!(stdout, "Visible body");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("'#secret'"), "{stderr}");

    // --keep-hidden なら取り出す
    let stdout = assert_success(&run_get_md_on(&page, &["-s", "#secret", "--keep-hidden"]));
    assert_eq!(stdout, "Secret text");
}

#[test]
#[ignore] // システムに Chrome/Chromium が必要
fn http_error_status_cannot_be_spoofed_by_page_script() {
    let url = static_http_url(
        404,
        r#"<!doctype html>
<html>
  <body>
    <script>
      performance.getEntriesByType = () => [{ responseStatus: 200 }];
    </script>
    <main>spoofed 404 body</main>
  </body>
</html>"#,
    );

    let output = get_md_bin()
        .args([
            url,
            "-s".to_string(),
            "main".to_string(),
            "-w".to_string(),
            "0".to_string(),
            "-q".to_string(),
        ])
        .output()
        .expect("Failed to execute get-md");

    assert!(
        !output.status.success(),
        "get-md should reject a real HTTP 404 even if page script spoofs PerformanceNavigationTiming: {}",
        String::from_utf8_lossy(&output.stdout),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("HTTP 404"),
        "stderr should report the real HTTP status: {stderr}",
    );
}
