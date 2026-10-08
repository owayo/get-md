//! 複数のモジュールのテストで共有する補助。

use std::path::PathBuf;

use crate::front_matter::{MetaEntry, SourceMetadata};

/// テストごとに重ならない一時ディレクトリのパスを返す (ディレクトリは作らない)。
pub(crate) fn make_temp_dir(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("failed to get current time")
            .as_nanos()
    ))
}

/// 項目の順序と省略を確かめやすい、テスト用の取得元の情報。
pub(crate) fn sample_metadata() -> SourceMetadata {
    SourceMetadata {
        url: "https://example.com/docs".to_string(),
        final_url: Some("https://example.com/docs/intro".to_string()),
        title: Some("Example Docs".to_string()),
        selectors: vec!["article".to_string(), "main".to_string()],
        retrieved_at: "2026-09-29T03:10:00Z".to_string(),
        meta: vec![meta_entry("source", "manual"), meta_entry("note", "")],
    }
}

/// `--meta` の項目を 1 つ作る。
pub(crate) fn meta_entry(key: &str, value: &str) -> MetaEntry {
    MetaEntry {
        key: key.to_string(),
        value: value.to_string(),
    }
}
