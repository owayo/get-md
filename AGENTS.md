# get-md

URL をブラウザで取得し、指定要素を Markdown に変換する CLI ツール。

## Tech Stack

- Rust (Edition 2024)
- headless_chrome (CDP 経由のブラウザ制御)
- htmd (HTML -> Markdown 変換、skip_tags/spacing オプション使用)
- clap (CLI 引数解析、derive feature)
- indicatif (プログレス表示)
- regex (日時パターンマッチング)
- url (相対URL -> 絶対URL 変換)
- anyhow (エラーハンドリング)

## Architecture

- WebDriver を使用せず、システムの Chrome/Chromium を CDP で直接制御
- JS レンダリング対応（SPA、動的コンテンツ）
- `--no-cache` で CDP の `Network.setCacheDisabled` によりブラウザキャッシュを無効化
- HTTPS 証明書は既定で検証し、信頼済みデバッグ用途のみ `--ignore-certificate-errors` で無視可能
- HTTP ステータスはページ内 JS ではなく CDP の Network response event から取得し、ページスクリプトによる偽装を防ぐ
- CSS セレクタでブラウザ内 JS 実行により要素の outerHTML を取得
- htmd で HTML -> Markdown 変換（script, style, noscript, svg は skip_tags で除去）
- Rust 側で相対 URL を絶対パスに変換（基準URLはレンダリング後の `document.baseURI` を使用し、`<base href>` とリダイレクト後URLに対応。Markdown リンク・画像の `[text](url)` パターン、`[text](<url>)` 形式、リンク先 URL 直前に空白がある形式、`\(` `\)` を含むリンク先の解析、`<...>` 内の `)` を終端として誤認しない解析、`<...>` 内の `\>` をエスケープとして正しく処理、通常リンク先URL内のクォート保持、`\ ` を含む通常リンク先の解析に対応。`Url::join` 前に `\(` `\)` `\>` `\ ` を実文字へ戻してから絶対 URL 化する。インラインコードとコードフェンス内（ブロッククォート内のフェンスを含む）は変換せず、未閉鎖のインラインバッククォートはリテラルとして扱い、実際のリンク/画像構文ではない単独の `](` は無視する。閉じ `)` が見つからない壊れたリンク候補があっても、走査を継続して後続の正常なリンクも解決する）
- テーブルのセルパディングとセパレータダッシュを圧縮（コードフェンス内は変更しない）。セル内のエスケープ済みパイプ（`\|`）は区切りとして扱わず、テーブル本文の `--` や `:` のようなセパレータ風データセルも保持する
- 対応 OS: macOS, Windows

## Project Structure

```
src/
  main.rs       # CLI 定義、ブラウザ起動、HTML 取得、Markdown 変換
  progress.rs   # indicatif ベースのプログレス表示
Makefile        # build, release, test, fmt, check, install ターゲット
.github/
  workflows/
    ci.yml      # CI (test, clippy, fmt, build)
    release.yml # リリース (バージョンバンプ、ビルド、GitHub Release、Homebrew更新)
```

## Development

```bash
make build    # デバッグビルド
make release  # リリースビルド
make test     # テスト
make check    # clippy + check
make fmt      # フォーマット
make install  # /usr/local/bin にインストール
```

## Testing

- ユニットテストは Chrome 不要（CLI パース、証明書エラー無視オプションのデフォルト安全性、JS エスケープ、Markdown変換、URL解決、リンクパーサー、コード領域を除外する URL 解決、未閉鎖バッククォートを含むリンク解決、フェンスマーカー検出、テーブルセル分割、ファイルステータス判定（削除済み tracked ファイルの同内容復元を含む実フロー再現）、日時無視比較、エスケープ判定、開き括弧検出、リンク候補検出、インラインコード閉じ検出、マルチバイト文字対応、フェンスコード直後のリンク解決、インラインコードとリンクの混在、プログレス表示（スピナー置き換え含む）、CRLF正規化、ネストフェンスのスキップ、画像リンク検出、リンク先分割の境界条件、連続テーブル圧縮、連続山括弧リンク解決、URL解決のエラー分岐、タイトル内エスケープ引用符、未閉鎖タイトル、ネスト括弧内の引用符、CRLFフェンスブロック、コロンのみセパレータ、セパレータ風データセル保持、行途中開始のインラインコード検出、テーブル行中央揃え・幅広パディング圧縮、タイムアウト境界値、ファイルステータス等値比較、ネストフェンス長さ不一致、多段相対パス解決、複数リンク候補検出、山括弧リンク先フォールバック、4段ネスト括弧、日時除去単独日付、無効日付パターン、日付バウンダリ境界、深ネスト引用符、altなし画像URL解決、双方非UTF-8日時比較、タブパディング圧縮、ブロッククォート内リンク解決、ブロッククォート内コードフェンスのURL解決除外、時刻のみパターン除外、標準・山括弧混在リンク、タブ区切りリンク先、末尾バックスラッシュ単独リンク先、改行直後フェンス開始判定、リンクテキスト内括弧、フェンス直後空行テーブル圧縮、リンク先 URL 直前の空白を含む標準・山括弧リンクの URL 解決、`\ ` `\(` `\)` `\>` を含むリンク先を絶対 URL 化する回帰テスト、壊れたリンク候補の後続リンク解決（標準・画像・山括弧・タイトル付きの4バリエーション）の回帰テスト、ブロッククォート対応フェンスマーカー検出関数の直接テスト（多段、空白なし、長いマーカー、空行）、リンク先エスケープ展開関数の直接テスト（`\ ` `\(` `\)` `\>` 個別、複数連続、マルチバイト混在、エスケープ対象外バックスラッシュの保持）、ブロッククォート行内テーブルが圧縮対象外であることの確認、ブロッククォート内フェンスコード内のテーブル行保持）
- E2E テストは実際の Chrome/Chromium が必要（`#[ignore]` 付き）。GitHub Raw の実取得、ローカル `file://` ページでの相対 URL 解決、`<base href>` による基準URL解決、複数セレクタの `---` 結合、`--ignore-date` の書き込み抑止、ページスクリプトが Performance API を偽装しても実 HTTP 404 を拒否することを確認する。一時ディレクトリは時刻・プロセス ID・プロセス内連番で一意化し、並列実行時の衝突を防ぐ
- `make test` または `cargo test` で実行

## Key Design Decisions

- Chrome/Chromium はシステムにインストール済みであることを前提とする
- HTTPS 証明書エラーは既定で無視しない。必要な場合のみ `--ignore-certificate-errors` で明示する
- セレクタ未指定時は body 全体を対象とする
- 複数セレクタ指定時は `---` で区切って結合
- ファイル出力時は末尾改行を保証
- 完了表示は出力書き込み成功後にのみ表示する（✨ created / 📝 updated / ✔ unchanged）
- ファイルステータス判定: 新規→created、内容変更→updated、同一内容→unchanged。git管理下で未ステージ変更がある場合は常にupdated。既存ファイルの読み取りに失敗した場合はupdated。ファイルの存在状態と書き込み前の未ステージ状態は `File::create` 前に記録し、書き込み後の `path.exists()` や diff 消失に依存しない。git 判定は対象パスに最も近い既存ディレクトリを起点に行い、削除済みの tracked ファイルや repo 外 cwd からの実行でも契約を守る
- `--ignore-date`: 日時パターン（`YYYY-MM-DD HH:MM(:SS)?`、スラッシュ区切り、`Z`・小数秒・タイムゾーン付き ISO 8601）を無視してファイル比較し、日時だけの差分なら上書きせず unchanged 扱いにする。双方に日時パターンを含む場合のみ比較し、非 UTF-8 や日時パターンを含まない場合は安全のため通常比較にフォールバックする。git 管理下の未ステージ変更がある場合は `file_status` と同じ契約で updated 扱い
- `idle_browser_timeout` は `timeout + 30s` のバッファを saturating 加算で設定する
- バージョニングは CalVer (YY.M.counter) 形式
