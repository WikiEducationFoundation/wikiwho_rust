//! `rebuild_rev_index` — admin tool that scans the storage tree and
//! produces a fresh `<volume>/<language>/rev_id_index.bin` per
//! language. Used to bootstrap the sidecar for storage trees that
//! pre-date the index (or to recover from corruption).
//!
//! Usage:
//!
//! ```text
//! rebuild_rev_index <volume>            # rebuild all languages under <volume>
//! rebuild_rev_index <volume> <language> # rebuild a single language
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use wikiwho_storage::rebuild::{discover_languages, rebuild_one_language};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let volume: PathBuf = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: rebuild_rev_index <volume> [<language>]"))?;
    let explicit_language = args.next();
    if args.next().is_some() {
        bail!("usage: rebuild_rev_index <volume> [<language>]");
    }

    if !volume.is_dir() {
        bail!("volume {} is not a directory", volume.display());
    }

    let languages: Vec<String> = match explicit_language {
        Some(lang) => vec![lang],
        None => discover_languages(&volume)?,
    };

    if languages.is_empty() {
        eprintln!("no language directories found under {}", volume.display());
        return Ok(());
    }

    for language in &languages {
        let lang_dir = volume.join(language);
        if !lang_dir.is_dir() {
            eprintln!("skipping {}: not a directory", lang_dir.display());
            continue;
        }
        let stats = rebuild_one_language(&volume, language)
            .with_context(|| format!("rebuilding {language}"))?;
        println!(
            "{language}: indexed {} rev_ids across {} articles",
            stats.entries, stats.articles
        );
    }

    Ok(())
}
