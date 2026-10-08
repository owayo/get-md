//! 出力の書き込みと、既存のファイルを書き換えるかの判定。
//!
//! ファイルへは一時ファイルに書いてから rename で置き換え、書き込み中の失敗で既存の
//! ファイルを壊さない。完了表示の状態 (created・updated・unchanged) は、書き込む前に
//! 記録したファイルの有無と git の未ステージの変更から決める。

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::front_matter::{mask_retrieved_at, split_front_matter};

/// 書き込む前に記録した出力先の状態。
pub(crate) struct OutputWriteState {
    pub(crate) old_content: Option<Vec<u8>>,
    pub(crate) file_existed_before: bool,
    pub(crate) had_unstaged_changes_before: bool,
}

/// 出力する内容を組み立てる。
///
/// front matter を付けるときは本文の前に置く。標準出力にもファイルにも同じ内容を出し、
/// ファイルへ出すときだけ末尾の改行を保証する (`finalize_output_text`)。
pub(crate) fn build_output_text(
    markdown: String,
    front_matter: Option<&str>,
    file_output: bool,
) -> String {
    let text = match front_matter {
        Some(header) => format!("{header}{markdown}"),
        None => markdown,
    };
    finalize_output_text(text, file_output)
}

fn finalize_output_text(markdown: String, file_output: bool) -> String {
    // 出力内容を確定する（末尾改行を保証）
    if file_output && !markdown.ends_with('\n') {
        format!("{markdown}\n")
    } else {
        markdown
    }
}

pub(crate) fn capture_output_write_state(output: Option<&Path>) -> OutputWriteState {
    // 通常ファイルだけを読む。FIFO や端末を読むと書き込み側や入力を待って止まり、
    // atomic_write の「通常ファイルでない」エラーまで進めない。
    let old_content = output
        .filter(|p| fs::metadata(p).is_ok_and(|metadata| metadata.is_file()))
        .and_then(|p| fs::read(p).ok());
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

/// 出力を書き込み、明示的に flush してから成功を返す。
///
/// `io::stdout()` は行バッファ（`LineWriter`）なので、最後の改行より後ろは
/// `write_all` だけでは実際に書き出されずバッファに残る。標準出力へ出すときは
/// `finalize_output_text` が末尾改行を付けないため必ず残り、flush しないと
/// 書き込み失敗（パイプ切断やディスク満杯）を握りつぶして「成功」表示のまま
/// 終了コード 0 で終わってしまう。プロセス終了時の暗黙の flush は結果を捨てる。
pub(crate) fn write_all_and_flush(mut writer: impl Write, bytes: &[u8]) -> Result<()> {
    writer.write_all(bytes).context("Failed to write output")?;
    writer.flush().context("Failed to flush output")?;
    Ok(())
}

/// 既存のファイルを書き換えずに残すか (内容が実質的に同じか) を判定する。
///
/// `front_matter` のとき、既存のファイルにも front matter があれば、retrieved_at の値を除いた
/// front matter と本文で比べる (`front_matter_output_matches`)。そうでなければ従来どおり、
/// `ignore_date` のときだけ日時だけの差分を同じとみなす。
pub(crate) fn keeps_existing_output(
    old: &[u8],
    new: &str,
    front_matter: bool,
    ignore_date: bool,
) -> bool {
    if front_matter && let Some(matches) = front_matter_output_matches(old, new, ignore_date) {
        return matches;
    }
    ignore_date && is_date_only_change(old, new.as_bytes())
}

/// front matter 付きの新しい出力を、既存のファイルと比べる。
///
/// front matter は retrieved_at の値だけを除いて文字列どおりに比べる (title・url・selectors・
/// --meta の変化は見逃さない)。本文は `ignore_date` なら日時だけの差分を同じとみなし
/// (`is_date_only_change`)、そうでなければ完全に一致したときだけ同じとみなす。
/// 既存のファイルが UTF-8 でないか front matter がなければ None を返す (従来の比較に戻す)。
pub(crate) fn front_matter_output_matches(
    old: &[u8],
    new: &str,
    ignore_date: bool,
) -> Option<bool> {
    let old = std::str::from_utf8(old).ok()?;
    let (old_header, old_body) = split_front_matter(old)?;
    let (new_header, new_body) = split_front_matter(new)?;

    let headers_match = old_header
        .split_inclusive('\n')
        .map(mask_retrieved_at)
        .eq(new_header.split_inclusive('\n').map(mask_retrieved_at));
    if !headers_match {
        return Some(false);
    }

    Some(
        old_body == new_body
            || (ignore_date && is_date_only_change(old_body.as_bytes(), new_body.as_bytes())),
    )
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
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
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
    let existing_metadata = match fs::metadata(&write_path) {
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
            Some(metadata)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| {
                format!("Failed to read output file metadata: {}", path.display())
            });
        }
    };

    // 既存ファイルを置き換えるときは、一時ファイルを所有者だけが読み書きできるモードで作る。
    // 緩いモードで作ってから絞ると、その間に開かれた fd から書き込み後の内容を読まれる
    // (open(2) の権限検査は開いた時点だけで、後の chmod は開いている fd に効かない)。
    let (tmp_path, mut tmp_file) = create_temp_file(write_parent, existing_metadata.is_some())?;
    // 書き込み失敗時に一時ファイルを残さないようガードする。
    let mut guard = TempFileGuard {
        path: tmp_path,
        persisted: false,
    };

    // 内容を書き込む前にパーミッションを揃える。書き込み後に絞ると、既存ファイルが
    // 制限モード(0600 等)の場合に新しい内容が一時的に緩いモードで読める窓ができる。
    // 開いたファイルディスクリプタへの書き込み権限は open 時に確定しており、
    // 先に読み取り専用へ変更しても後続の write_all は失敗しない。
    if let Some(metadata) = existing_metadata {
        // モードの意味は所有グループで決まるので、グループも先に引き継ぐ (rename で作り直すと
        // 親ディレクトリか作成者のグループになり、0640 などで読める人が変わる)。所属していない
        // グループには変えられないため best-effort。chown が set-id ビットを落とすので chmod より前。
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = std::os::unix::fs::fchown(&tmp_file, None, Some(metadata.gid()));
        }
        tmp_file
            .set_permissions(metadata.permissions())
            .with_context(|| {
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
/// `owner_only` なら (Unix では) 作った時点から所有者だけが読み書きできるモードにする。
fn create_temp_file(parent: &Path, owner_only: bool) -> Result<(PathBuf, File)> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if owner_only {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = owner_only;

    for attempt in 0..1000u32 {
        let path = parent.join(format!(
            ".get-md-{}-{nanos}-{attempt}.tmp",
            std::process::id()
        ));
        match options.open(&path) {
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
pub(crate) fn file_status<'a>(
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

/// 書き換えずに残したファイルのステータスを判定する。
///
/// 内容は実質的に同じなので unchanged。ただし git 管理下で未ステージの変更があれば、
/// `file_status` と同じ契約で updated 扱いにする。
pub(crate) fn kept_file_status<'a>(
    path: &Path,
    had_unstaged_changes_before: bool,
) -> (&'a str, &'a str) {
    if had_unstaged_changes_before || has_unstaged_changes(path) {
        ("📝", "updated")
    } else {
        ("✔", "unchanged")
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
pub(crate) fn is_date_only_change(old: &[u8], new: &[u8]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::make_temp_dir;
    use std::io;

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

    /// 書き込み・flush の成否を個別に制御できるテスト用ライタ。
    struct FlakyWriter {
        buffer: Vec<u8>,
        write_error: Option<ErrorKind>,
        flush_error: Option<ErrorKind>,
    }

    impl FlakyWriter {
        fn ok() -> Self {
            Self {
                buffer: Vec::new(),
                write_error: None,
                flush_error: None,
            }
        }

        fn failing_flush(kind: ErrorKind) -> Self {
            Self {
                buffer: Vec::new(),
                write_error: None,
                flush_error: Some(kind),
            }
        }

        fn failing_write(kind: ErrorKind) -> Self {
            Self {
                buffer: Vec::new(),
                write_error: Some(kind),
                flush_error: None,
            }
        }
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self.write_error {
                Some(kind) => Err(io::Error::new(kind, "write failed")),
                None => {
                    self.buffer.extend_from_slice(buf);
                    Ok(buf.len())
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.flush_error {
                Some(kind) => Err(io::Error::new(kind, "flush failed")),
                None => Ok(()),
            }
        }
    }

    #[test]
    fn write_all_and_flush_writes_all_bytes() {
        let mut writer = FlakyWriter::ok();
        write_all_and_flush(&mut writer, b"no trailing newline").unwrap();
        assert_eq!(writer.buffer, b"no trailing newline");
    }

    #[test]
    fn write_all_and_flush_propagates_flush_error() {
        // 行バッファのままだと最終行が書き出されないので、flush の失敗は
        // 握りつぶさずに呼び出し元へ伝える必要がある。
        let err = write_all_and_flush(FlakyWriter::failing_flush(ErrorKind::BrokenPipe), b"body")
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to flush output"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_all_and_flush_propagates_write_error() {
        let err = write_all_and_flush(
            FlakyWriter::failing_write(ErrorKind::PermissionDenied),
            b"body",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("Failed to write output"),
            "unexpected error: {err}"
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

    #[cfg(unix)]
    #[test]
    fn capture_output_write_state_does_not_block_on_fifo() {
        let dir = make_temp_dir("get-md-output-state-fifo");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let fifo = dir.join("out.md");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("failed to run mkfifo");
        assert!(status.success());

        let (tx, rx) = std::sync::mpsc::channel();
        let fifo_for_thread = fifo.clone();
        std::thread::spawn(move || {
            let state = capture_output_write_state(Some(&fifo_for_thread));
            let _ = tx.send((state.old_content.is_none(), state.file_existed_before));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        if result.is_err() {
            // 止まったスレッドの open を、書き込み側を開いて解放する
            let _ = OpenOptions::new().write(true).open(&fifo);
        }
        // FIFO は読まず (書き込み側を待たず)、存在する出力先として記録する
        assert_eq!(result, Ok((true, true)));
        // 続く atomic_write は通常ファイルでない出力先をエラーにする
        assert!(atomic_write(&fifo, b"content").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_output_text_without_front_matter_is_unchanged() {
        assert_eq!(build_output_text("Body".to_string(), None, false), "Body");
        assert_eq!(build_output_text("Body".to_string(), None, true), "Body\n");
    }

    #[test]
    fn keeps_existing_output_without_front_matter_follows_previous_rules() {
        let old = b"Updated: 2026-04-12 09:00\n";
        assert!(keeps_existing_output(
            old,
            "Updated: 2026-04-13 10:00\n",
            false,
            true
        ));
        assert!(!keeps_existing_output(
            old,
            "Updated: 2026-04-13 10:00\n",
            false,
            false
        ));
        // 同じ内容は従来どおり書き込みへ進む (日時だけの差分とはみなさない)
        assert!(!keeps_existing_output(
            old,
            "Updated: 2026-04-12 09:00\n",
            false,
            true
        ));
    }

    #[test]
    fn kept_file_status_is_unchanged_outside_git() {
        let dir = make_temp_dir("get-md-kept-status");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("output.md");
        std::fs::write(&path, b"content").expect("failed to write fixture file");

        assert_eq!(kept_file_status(&path, false), ("✔", "unchanged"));
        assert_eq!(kept_file_status(&path, true), ("📝", "updated"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kept_file_status_is_updated_with_unstaged_changes() {
        let dir = make_temp_dir("get-md-kept-status-git");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("tracked.md");

        git(&dir, &["init"]);
        git(&dir, &["config", "user.name", "Test User"]);
        git(&dir, &["config", "user.email", "test@example.com"]);
        std::fs::write(&path, b"old").expect("failed to write tracked file");
        git(&dir, &["add", "tracked.md"]);
        git(&dir, &["commit", "-m", "init"]);
        std::fs::write(&path, b"new").expect("failed to update tracked file");

        assert_eq!(kept_file_status(&path, false), ("📝", "updated"));

        let _ = std::fs::remove_dir_all(&dir);
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
    fn create_temp_file_for_replacement_is_owner_only_from_creation() {
        use std::os::unix::fs::PermissionsExt;

        // 置き換え用の一時ファイルは、作った時点で group / other が開けないこと。
        // 緩いモードで作ってから chmod すると、その間に開かれた fd から後で書く内容を読まれる。
        let dir = make_temp_dir("get-md-temp-owner-only");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let (path, file) = create_temp_file(&dir, true).expect("temp file");
        let mode = file.metadata().unwrap().permissions().mode() & 0o777;
        drop(file);
        assert_eq!(mode & 0o077, 0, "mode at creation: {mode:o}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_group_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = make_temp_dir("get-md-atomic-group");
        std::fs::create_dir_all(&dir).expect("failed to create temp dir");
        let path = dir.join("out.md");
        std::fs::write(&path, b"old").expect("failed to write fixture");

        // ディレクトリのグループとも、作成者の主グループとも違う、所属しているグループを使う
        let dir_gid = std::fs::metadata(&dir).unwrap().gid();
        let primary_gid = std::fs::metadata(&path).unwrap().gid();
        let groups = Command::new("id")
            .arg("-G")
            .output()
            .expect("failed to run id");
        let Some(other_gid) = String::from_utf8_lossy(&groups.stdout)
            .split_whitespace()
            .filter_map(|gid| gid.parse::<u32>().ok())
            .find(|&gid| gid != dir_gid && gid != primary_gid)
        else {
            // 補助グループに所属していない環境では確かめられない
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        std::os::unix::fs::chown(&path, None, Some(other_gid)).expect("failed to change group");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))
            .expect("failed to set permissions");

        atomic_write(&path, b"new").expect("atomic_write should succeed");

        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(
            metadata.gid(),
            other_gid,
            "0640 のまま読めるグループが変わってはいけない"
        );
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
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
        } // ここで Drop が実行される
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
        let (p1, f1) = create_temp_file(&dir, false).expect("first temp file");
        let (p2, f2) = create_temp_file(&dir, false).expect("second temp file");
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
        let (path, file) = create_temp_file(&dir, false).expect("temp file");
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

    #[test]
    fn is_date_only_change_identical_dates_different_text() {
        // 日時は同じだが他のテキストが異なる場合は date-only change ではない
        let old = b"Updated: 2025-01-01 12:00 - version A";
        let new = b"Updated: 2025-01-01 12:00 - version B";
        assert!(!is_date_only_change(old, new));
    }

    #[test]
    fn is_date_only_change_single_date_in_large_text() {
        // 大きなテキスト中の一箇所だけ日時が変わった場合
        let old = b"Header\nContent line 1\nDate: 2025-01-01 10:00\nContent line 2\nFooter";
        let new = b"Header\nContent line 1\nDate: 2025-06-15 14:30\nContent line 2\nFooter";
        assert!(is_date_only_change(old, new));
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

    // --- is_date_only_change: 日時除去後に空白パターンが異なる ---

    #[test]
    fn date_only_change_whitespace_diff_after_strip_returns_false() {
        // 日時を除去した後の空白パターンが異なる → date-only ではない
        let old = b"A 2024-01-01 B";
        let new = b"A  2024-02-02 B";
        assert!(!is_date_only_change(old, new));
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
}
