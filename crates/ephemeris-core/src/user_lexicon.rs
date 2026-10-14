//! User-vocabulary lexicon for HTR beam decoding (ephemeris-ktg.13).
//!
//! Builds a frequency-ranked token list from the Obsidian vault, task titles,
//! and calendar event titles. Persists under the app data dir and refreshes at
//! most once per day. Merged into [`crate::HtrLexicon`] above the bundled
//! generic lexicon.

use crate::note_search::{list_markdown_files, markdown_to_plain};
use crate::stroke_htr::{
    is_structured_htr_token, merged_htr_lexicon, tokenize_htr_text, HtrLexicon,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Cap on persisted user tokens (top-N by frequency).
pub const USER_LEXICON_MAX_TOKENS: usize = 20_000;
/// On-disk word list (one token per line).
pub const USER_LEXICON_FILENAME: &str = "user_htr_lexicon.txt";
/// Minimum interval between automatic rebuilds.
pub const USER_LEXICON_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Inputs for a user-lexicon rebuild.
#[derive(Debug, Clone, Default)]
pub struct UserLexiconSources {
    pub vault_paths: Vec<PathBuf>,
    /// Vault-relative export tree to skip (e.g. `Ephemeris`).
    pub exclude_subdir: Option<PathBuf>,
    pub task_titles: Vec<String>,
    pub event_titles: Vec<String>,
}

/// Path of the persisted user lexicon under `data_dir`.
pub fn user_lexicon_path(data_dir: &Path) -> PathBuf {
    data_dir.join(USER_LEXICON_FILENAME)
}

/// True when the on-disk lexicon is missing or older than the daily refresh window.
pub fn user_lexicon_needs_refresh(data_dir: &Path, now: SystemTime) -> bool {
    let path = user_lexicon_path(data_dir);
    let Ok(meta) = fs::metadata(&path) else {
        return true;
    };
    let Ok(mtime) = meta.modified() else {
        return true;
    };
    match now.duration_since(mtime) {
        Ok(age) => age >= USER_LEXICON_REFRESH_INTERVAL,
        Err(_) => false, // mtime in the future — treat as fresh
    }
}

/// Read persisted user lexicon text, if any.
pub fn load_user_lexicon_text(data_dir: &Path) -> Option<String> {
    let path = user_lexicon_path(data_dir);
    fs::read_to_string(path)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Bundled + on-disk user vocabulary merged for CTC beam / cascade scoring.
pub fn load_merged_htr_lexicon(data_dir: Option<&Path>) -> HtrLexicon {
    let user = data_dir.and_then(load_user_lexicon_text);
    merged_htr_lexicon(user.as_deref())
}

/// Accumulate token frequencies from free text (vault bodies, titles, …).
pub fn accumulate_token_frequencies(text: &str, counts: &mut HashMap<String, u32>) {
    for tok in tokenize_htr_text(text) {
        if !retain_user_token(&tok) {
            continue;
        }
        *counts.entry(tok).or_insert(0) += 1;
    }
}

fn retain_user_token(tok: &str) -> bool {
    if tok.is_empty() {
        return false;
    }
    if is_structured_htr_token(tok) {
        return true;
    }
    // Drop ultra-short noise; keep German compounds / names (len ≥ 2).
    tok.chars().count() >= 2
}

/// Build frequency-ranked tokens (desc) capped at [`USER_LEXICON_MAX_TOKENS`].
pub fn build_user_lexicon_tokens(sources: &UserLexiconSources) -> Vec<(String, u32)> {
    let mut counts: HashMap<String, u32> = HashMap::new();

    for vault in &sources.vault_paths {
        if !vault.is_dir() {
            continue;
        }
        let exclude = sources.exclude_subdir.as_deref();
        let Ok(files) = list_markdown_files(vault, exclude) else {
            continue;
        };
        for path in files {
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            let plain = markdown_to_plain(&body);
            accumulate_token_frequencies(&plain, &mut counts);
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                accumulate_token_frequencies(stem, &mut counts);
            }
        }
    }

    for title in &sources.task_titles {
        accumulate_token_frequencies(title, &mut counts);
    }
    for title in &sources.event_titles {
        accumulate_token_frequencies(title, &mut counts);
    }

    let mut ranked: Vec<(String, u32)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(USER_LEXICON_MAX_TOKENS);
    ranked
}

/// Persist top tokens as a newline-separated word list.
pub fn save_user_lexicon(data_dir: &Path, tokens: &[(String, u32)]) -> crate::Result<()> {
    fs::create_dir_all(data_dir).map_err(crate::AppError::Io)?;
    let path = user_lexicon_path(data_dir);
    let mut out = String::from("# User HTR lexicon (ephemeris-ktg.13). Top tokens by frequency.\n");
    for (tok, _n) in tokens {
        out.push_str(tok);
        out.push('\n');
    }
    fs::write(&path, out).map_err(crate::AppError::Io)?;
    Ok(())
}

/// Rebuild from `sources` and write to `data_dir`. Returns token count.
pub fn build_and_save_user_lexicon(
    data_dir: &Path,
    sources: &UserLexiconSources,
) -> crate::Result<usize> {
    let tokens = build_user_lexicon_tokens(sources);
    let n = tokens.len();
    save_user_lexicon(data_dir, &tokens)?;
    Ok(n)
}

/// Rebuild when stale (or missing). Returns `Some(n)` when a rebuild ran.
pub fn refresh_user_lexicon_if_stale(
    data_dir: &Path,
    sources: &UserLexiconSources,
    now: SystemTime,
) -> crate::Result<Option<usize>> {
    if !user_lexicon_needs_refresh(data_dir, now) {
        return Ok(None);
    }
    let n = build_and_save_user_lexicon(data_dir, sources)?;
    Ok(Some(n))
}

/// Spawn a background rebuild (never call from the UI thread's hot path).
///
/// On completion, updates `shared` with the merged lexicon when provided.
pub fn spawn_user_lexicon_refresh(
    data_dir: PathBuf,
    sources: UserLexiconSources,
    shared: Option<std::sync::Arc<std::sync::RwLock<HtrLexicon>>>,
) {
    std::thread::spawn(move || {
        let now = SystemTime::now();
        match refresh_user_lexicon_if_stale(&data_dir, &sources, now) {
            Ok(Some(n)) => {
                tracing::info!(
                    "User HTR lexicon refreshed ({n} tokens) → {}",
                    data_dir.display()
                );
            }
            Ok(None) => {
                tracing::debug!("User HTR lexicon still fresh under {}", data_dir.display());
            }
            Err(e) => {
                tracing::warn!("User HTR lexicon refresh failed: {e}");
                return;
            }
        }
        if let Some(shared) = shared {
            let merged = load_merged_htr_lexicon(Some(&data_dir));
            if let Ok(mut guard) = shared.write() {
                *guard = merged;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stroke_htr::{GENERIC_LEXICON_WEIGHT, USER_LEXICON_WEIGHT};
    use std::io::Write;
    use tempfile::TempDir;

    fn write_md(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn build_ranks_frequent_tokens_from_sample_texts() {
        let tmp = TempDir::new().unwrap();
        write_md(
            tmp.path(),
            "Projects/Alpha.md",
            "# Alpha\n\nMeet about Ephemeris Ephemeris ink tomorrow at 10:00 @phone #lane/now",
        );
        write_md(tmp.path(), "Inbox.md", "# Inbox\n\nBuy milk");

        let sources = UserLexiconSources {
            vault_paths: vec![tmp.path().to_path_buf()],
            exclude_subdir: None,
            task_titles: vec![
                "Ephemeris release checklist @computer".into(),
                "Call bank #lane/next".into(),
            ],
            event_titles: vec!["Ephemeris sync 13.10.".into()],
        };
        let tokens = build_user_lexicon_tokens(&sources);
        assert!(!tokens.is_empty());
        let map: HashMap<_, _> = tokens.into_iter().collect();
        assert!(
            map.get("ephemeris").copied().unwrap_or(0) >= 3,
            "ephemeris should dominate: {map:?}"
        );
        assert!(map.contains_key("10:00") || map.contains_key("@phone"));
        assert!(map.contains_key("tomorrow") || map.contains_key("morgen"));
    }

    #[test]
    fn merge_weights_prefer_user_over_generic() {
        let mut lex = HtrLexicon::from_word_list("the\nmeeting\n");
        let user = HtrLexicon::from_word_list_weighted("ephemeris\nmeeting\n", USER_LEXICON_WEIGHT);
        lex.merge(&user);
        assert!((lex.weight_of("the") - GENERIC_LEXICON_WEIGHT).abs() < f32::EPSILON);
        assert!((lex.weight_of("ephemeris") - USER_LEXICON_WEIGHT).abs() < f32::EPSILON);
        assert!(
            lex.weight_of("meeting") >= USER_LEXICON_WEIGHT - f32::EPSILON,
            "user weight should win on overlap"
        );
        assert!(lex.hit_ratio("ephemeris") > lex.hit_ratio("the"));
    }

    #[test]
    fn structured_tokens_score_without_map_entry() {
        let lex = HtrLexicon::default();
        assert!(crate::stroke_htr::is_structured_htr_token("10:00"));
        assert!(crate::stroke_htr::is_structured_htr_token("13.10."));
        assert!(crate::stroke_htr::is_structured_htr_token("morgen"));
        assert!(crate::stroke_htr::is_structured_htr_token("tomorrow"));
        assert!(crate::stroke_htr::is_structured_htr_token("@phone"));
        assert!(crate::stroke_htr::is_structured_htr_token("#lane/now"));
        let ratio = lex.hit_ratio("meet @phone at 10:00 morgen #lane/now");
        assert!(
            ratio > 0.5,
            "structured tokens should count as hits: {ratio}"
        );
    }

    #[test]
    fn persist_and_load_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let tokens = vec![("ephemeris".into(), 5u32), ("ink".into(), 3)];
        save_user_lexicon(tmp.path(), &tokens).unwrap();
        let text = load_user_lexicon_text(tmp.path()).expect("text");
        assert!(text.contains("ephemeris"));
        let merged = load_merged_htr_lexicon(Some(tmp.path()));
        assert!(merged.weight_of("ephemeris") >= USER_LEXICON_WEIGHT - f32::EPSILON);
        assert!(merged.weight_of("the") >= GENERIC_LEXICON_WEIGHT - f32::EPSILON);
    }

    #[test]
    fn refresh_skips_when_fresh() {
        let tmp = TempDir::new().unwrap();
        let sources = UserLexiconSources {
            task_titles: vec!["unique-token-xyz".into()],
            ..Default::default()
        };
        let n = build_and_save_user_lexicon(tmp.path(), &sources).unwrap();
        assert!(n >= 1);
        let again = refresh_user_lexicon_if_stale(tmp.path(), &sources, SystemTime::now()).unwrap();
        assert!(again.is_none(), "same-day refresh should no-op");
    }
}
