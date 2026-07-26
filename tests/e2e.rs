use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

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
