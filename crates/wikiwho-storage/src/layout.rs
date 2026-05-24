//! Directory layout for the on-disk blob (STORAGE.md §1).
//!
//! Per article we shard by `page_id`:
//!
//! ```text
//! <volume>/
//!   <lang>/
//!     <page_id // 1_000_000>/
//!       <page_id // 1000>/
//!         <page_id>/
//!           meta.json
//!           strings.bin
//!           tokens.bin
//!           revisions.bin
//!           hashtables.bin
//! ```
//!
//! Two-level sharding keeps any one directory at well under 1000
//! entries even with millions of articles per wiki.

use std::path::{Path, PathBuf};

/// Filenames used inside an article directory. Centralized so the
/// reader and writer stay consistent.
pub const STRINGS_FILE: &str = "strings.bin";
pub const TOKENS_FILE: &str = "tokens.bin";
pub const REVISIONS_FILE: &str = "revisions.bin";
pub const PARAGRAPHS_FILE: &str = "paragraphs.bin";
pub const SENTENCES_FILE: &str = "sentences.bin";
pub const HASHTABLES_FILE: &str = "hashtables.bin";
pub const META_FILE: &str = "meta.json";

/// Compute the article directory path under a volume + language root.
///
/// `volume` is typically `/blobs/<lang>` in production, or a temp dir
/// in tests. `language` is the wiki code (e.g. `"en"`, `"simple"`).
pub fn article_dir(volume: &Path, language: &str, page_id: u64) -> PathBuf {
    let major = page_id / 1_000_000;
    let minor = page_id / 1_000;
    volume
        .join(language)
        .join(major.to_string())
        .join(minor.to_string())
        .join(page_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn shards_obama_correctly() {
        // page_id 534366 → 0 / 534 / 534366
        let p = article_dir(&PathBuf::from("/blobs"), "en", 534366);
        assert_eq!(p, PathBuf::from("/blobs/en/0/534/534366"));
    }

    #[test]
    fn shards_huge_page_id() {
        // page_id 12_345_678 → 12 / 12345 / 12345678
        let p = article_dir(&PathBuf::from("/blobs"), "en", 12_345_678);
        assert_eq!(p, PathBuf::from("/blobs/en/12/12345/12345678"));
    }

    #[test]
    fn shards_small_page_id() {
        // page_id 1 → 0 / 0 / 1
        let p = article_dir(&PathBuf::from("/data"), "simple", 1);
        assert_eq!(p, PathBuf::from("/data/simple/0/0/1"));
    }

    #[test]
    fn keeps_directory_size_bounded() {
        // The middle shard groups ~1000 articles. Across a wiki with
        // 10M articles, the major shard has 10 entries; each middle
        // shard has up to 1000 entries. Spot-check this is true.
        let p1 = article_dir(&PathBuf::from("/x"), "en", 999);
        let p2 = article_dir(&PathBuf::from("/x"), "en", 1000);
        // p1 is shard 0/0/999; p2 is shard 0/1/1000 — different middle
        // shard.
        assert!(p1.to_string_lossy().contains("/0/0/"));
        assert!(p2.to_string_lossy().contains("/0/1/"));
    }
}
