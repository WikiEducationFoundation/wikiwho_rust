//! `whocolor` endpoints (API.md §7-8).
//!
//! These serve the per-token authorship rendering used by Dashboard's
//! ArticleViewer and the WhoWroteThat gadget. URL shapes:
//!
//! - `/{lang}/whocolor/v1.0.0-beta/{title}/{rev_id}/` — specific rev.
//! - `/{lang}/whocolor/v1.0.0-beta/{title}/` — latest processed rev.
//!
//! Per API.md §7 the `rev_id == 0` form is a workaround for titles
//! that contain `/`: clients pass `0` to mean "no rev_id given".
//!
//! Pipeline (cache hit):
//! 1. Resolve `title → page_id` from the on-disk title index.
//! 2. `SnapshotReader::open` → in-memory [`Article`].
//! 3. [`get_whocolor_data`] → token + revision data with
//!    conflict_score / age_seconds.
//! 4. In parallel: `mw.resolve_users` (editor user_id → display
//!    name) and `mw.fetch_revision_text` (raw wikitext for the
//!    rev_id).
//! 5. [`crate::whocolor_wikitext::inject_spans_into_wikitext`] →
//!    span markup injected at token byte positions in the
//!    wikitext, producing `present_editors` as a side-effect.
//! 6. `mw.parse_wikitext` POSTs the decorated wikitext to MW
//!    Action API `action=parse`; MW preserves the inline span
//!    tags through to the rendered HTML.
//! 7. Compose API.md §7 envelope.
//!
//! This is the wikitext-level injection flow that matches
//! production's WhoColor.parser.WikiMarkupParser. HTML-level
//! injection in `whocolor_html` is still kept for potential
//! future smart-extractor work but no production code path
//! uses it.
//!
//! Cache miss → spawn the same background fetcher the rev_content
//! cache-miss path uses and return the 200 "still processing"
//! envelope.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value, json};
use wikiwho_attribute::structures::Article;
use wikiwho_attribute::whocolor::{WhoColorData, WhoColorError, get_whocolor_data_now};
use wikiwho_mwclient::MwClient;
use wikiwho_storage::reader::SnapshotReader;

use crate::cache_miss;
use crate::error::ServerError;
use crate::state::AppState;
use crate::whocolor_html::token_class_name;
use crate::whocolor_wikitext::{WikitextToken, inject_spans_into_wikitext};

/// Path params for endpoint 7: `/{lang}/whocolor/{version}/{title}/{rev_id}/`.
#[derive(serde::Deserialize)]
pub struct WhoColorRevPath {
    pub lang: String,
    #[allow(dead_code)]
    pub version: String,
    pub title: String,
    pub rev_id: u64,
}

/// Path params for endpoint 8: `/{lang}/whocolor/{version}/{title}/`.
#[derive(serde::Deserialize)]
pub struct WhoColorLatestPath {
    pub lang: String,
    #[allow(dead_code)]
    pub version: String,
    pub title: String,
}

/// Query params for WhoColor. `origin=*` is sent by some consumers as
/// a CORS workaround; the handler ignores it but accepting it via
/// serde keeps axum's query parser happy.
#[derive(serde::Deserialize, Default)]
#[allow(dead_code)] // fields are deserialization sinks
pub struct WhoColorQuery {
    pub origin: Option<String>,
}

/// Endpoint 7: WhoColor for a specific rev_id.
///
/// `rev_id == 0` is the "title-with-slash" workaround: treat as
/// endpoint 8 (latest revision).
pub async fn whocolor_by_title_rev(
    State(state): State<AppState>,
    Path(path): Path<WhoColorRevPath>,
    Query(_query): Query<WhoColorQuery>,
) -> Response {
    let normalized_title = normalize_title(&path.title);
    if path.rev_id == 0 {
        return serve_whocolor(state, &path.lang, &normalized_title, None).await;
    }
    serve_whocolor(state, &path.lang, &normalized_title, Some(path.rev_id)).await
}

/// Endpoint 8: WhoColor for the latest processed revision.
pub async fn whocolor_by_title_latest(
    State(state): State<AppState>,
    Path(path): Path<WhoColorLatestPath>,
    Query(_query): Query<WhoColorQuery>,
) -> Response {
    let normalized_title = normalize_title(&path.title);
    serve_whocolor(state, &path.lang, &normalized_title, None).await
}

/// Shared service logic for both endpoints. `target_rev_id`:
/// `Some(N)` → render that rev (must be in the article's processed
/// set); `None` → latest processed rev (`ordered_revisions.last()`).
async fn serve_whocolor(
    state: AppState,
    lang: &str,
    title: &str,
    target_rev_id: Option<u64>,
) -> Response {
    // 1. Title → page_id. If the article isn't indexed locally, ask
    //    MW to resolve and spawn cache-miss.
    let page_id = match state.resolve_title(lang, title) {
        Some(pid) => pid,
        None => {
            return trigger_whocolor_cache_miss(state, lang, title, target_rev_id).await;
        }
    };

    // 2. Load the on-disk article.
    let article = match SnapshotReader::open(state.storage_root(), lang, page_id) {
        Ok(reader) => reader.article,
        Err(wikiwho_storage::StorageError::Io(io))
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            return trigger_whocolor_cache_miss(state, lang, title, target_rev_id).await;
        }
        Err(err) => return error_500(ServerError::from(err), title),
    };

    // 3. Determine the rev_id to render.
    let rev_id = match target_rev_id {
        Some(id) => id,
        None => match article.ordered_revisions.last().copied() {
            Some(id) => id,
            None => {
                return still_processing(title, target_rev_id);
            }
        },
    };

    // If the rev_id isn't in the processed set, the article was built
    // but doesn't include this revision. API.md §7 carries two
    // distinct envelopes:
    //   - "not yet" (still processing) — fallthrough below.
    //   - "vandalism detected" — used when the rev_id is on the
    //     article's spam list.
    if !article.revisions.contains_key(&rev_id) {
        if article.spam_ids.contains(&rev_id) {
            return vandalism_response(title, rev_id);
        }
        return still_processing(title, Some(rev_id));
    }

    // 4. Build the algorithm-output data.
    let data = match get_whocolor_data_now(&article, rev_id) {
        Ok(d) => d,
        Err(WhoColorError::UnknownRevision(_)) => {
            return still_processing(title, Some(rev_id));
        }
        Err(e) => return error_500(ServerError::Internal(e.to_string()), title),
    };

    // 5. Resolve editor display names + raw wikitext in parallel.
    //    The wikitext is the substrate we inject span markup into;
    //    the resolved usernames go into the response envelope.
    let mw = match state.mw_client(lang) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "mw client unavailable for whocolor");
            return error_500(
                ServerError::Internal(format!("MW client unavailable: {e}")),
                title,
            );
        }
    };
    let editor_ids = unique_registered_editor_ids(&data);
    let wikitext_future = mw.fetch_revision_text(rev_id);
    let users_future = mw.resolve_users(&editor_ids);
    let (wikitext_res, users_res) = tokio::join!(wikitext_future, users_future);

    let editor_names: HashMap<u64, String> = match users_res {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "resolve_users failed; proceeding with raw ids");
            HashMap::new()
        }
    };
    let wikitext = match wikitext_res {
        Ok(w) => w,
        Err(wikiwho_mwclient::MwError::PageMissing { .. }) => {
            return error_503(
                "MW returned no wikitext for this revision",
                title,
                Some(rev_id),
            );
        }
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "fetch_revision_text failed");
            return error_503(
                &format!("Wikitext fetch failed: {e}"),
                title,
                Some(rev_id),
            );
        }
    };

    // 6. Inject spans into the wikitext at each token's byte
    //    position. Mirrors production's WikiMarkupParser approach
    //    (see whocolor_wikitext.rs). spawn_blocking because the
    //    regex sweeps + token walk are CPU-bound.
    let injection_tokens: Vec<WikitextToken> = data
        .tokens
        .iter()
        .map(|t| WikitextToken {
            str: t.str.clone(),
            editor: t.editor.clone(),
            class_name: token_class_name(&t.editor),
        })
        .collect();
    let wikitext_clone = wikitext.clone();
    let injection = tokio::task::spawn_blocking(move || {
        inject_spans_into_wikitext(&wikitext_clone, &injection_tokens)
    })
    .await;
    let injection = match injection {
        Ok(i) => i,
        Err(join_err) => {
            return error_500(
                ServerError::Internal(format!(
                    "wikitext span injection task panicked: {join_err}"
                )),
                title,
            );
        }
    };

    // 7. POST the span-decorated wikitext through MW Action API
    //    `action=parse`. MW's parser carries the inline span tags
    //    through to the rendered HTML.
    let extended_html = match mw.parse_wikitext(title, &injection.wikitext).await {
        Ok(html) => html,
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "parse_wikitext failed");
            return error_503(
                &format!("Wikitext render failed: {e}"),
                title,
                Some(rev_id),
            );
        }
    };

    // Convert the wikitext module's PresentEditorEntry to the
    // envelope-building type (same shape, different module).
    let present_editors: Vec<crate::whocolor_html::PresentEditorEntry> = injection
        .present_editors
        .into_iter()
        .map(|e| crate::whocolor_html::PresentEditorEntry {
            editor: e.editor,
            class_name: e.class_name,
            token_count: e.token_count,
        })
        .collect();

    // 8. Compose the response envelope.
    let body = build_whocolor_envelope(
        &article,
        title,
        rev_id,
        &data,
        &editor_names,
        extended_html,
        &present_editors,
    );
    (StatusCode::OK, Json(body)).into_response()
}

/// Resolve an editor string to its display name. Mirrors
/// `whocolor/handler.py:117` (`editor_names_dict.get(editor, editor)`):
/// the raw editor string is the fallback. Registered users get looked
/// up by user_id; anons (`0|<ip>`) and unknown editors echo the input
/// verbatim — production keeps the `0|` prefix in the display name.
fn display_name(editor: &str, editor_names: &HashMap<u64, String>) -> String {
    if let Ok(id) = editor.parse::<u64>() {
        if let Some(name) = editor_names.get(&id) {
            return name.clone();
        }
    }
    editor.to_string()
}

/// Collect the unique registered user_ids from a `WhoColorData`'s
/// revisions list. Anons (editor starts with `0|`) and empty
/// editors are excluded — we have nothing to look up for them.
fn unique_registered_editor_ids(data: &WhoColorData) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for (_, rev) in &data.revisions {
        if let Ok(id) = rev.editor.parse::<u64>() {
            if seen.insert(id) {
                out.push(id);
            }
        }
    }
    out
}

/// Build the final API.md §7 wire-format envelope.
fn build_whocolor_envelope(
    _article: &Article,
    title: &str,
    rev_id: u64,
    data: &WhoColorData,
    editor_names: &HashMap<u64, String>,
    extended_html: String,
    present_editors: &[crate::whocolor_html::PresentEditorEntry],
) -> Value {
    // tokens: array-of-arrays per API.md §7.
    //   [conflict_score, str, o_rev_id, in, out, class_name, age_seconds]
    let tokens_json: Vec<Value> = data
        .tokens
        .iter()
        .map(|t| {
            let class_name = token_class_name(&t.editor);
            json!([
                t.conflict_score,
                t.str,
                t.o_rev_id,
                t.inbound,
                t.outbound,
                class_name,
                t.age_seconds,
            ])
        })
        .collect();

    // revisions: dict keyed by rev_id (as string), value is
    //   [timestamp, parent_id, class_name, editor_name].
    //
    // Anonymous editors keep the literal `0|<ip>` form in the
    // editor_name slot — production's `whocolor/handler.py:117` does
    // `editor_names_dict.get(rev_data[2], rev_data[2])`, which returns
    // the raw editor string when no mapping exists, prefix and all.
    let mut revisions_map = Map::new();
    for (rid, rev) in &data.revisions {
        let class_name = token_class_name(&rev.editor);
        let editor_name = display_name(&rev.editor, editor_names);
        revisions_map.insert(
            rid.to_string(),
            json!([
                rev.timestamp,
                rev.parent_rev_id,
                class_name,
                editor_name,
            ]),
        );
    }

    // present_editors: array of `[name, class_name, percentage]`
    // triples. The percentage is `token_count / total_present_tokens *
    // 100.0` per `WhoColor/parser.py:223-227`. API.md §7's example
    // shows 2-tuples but production has long emitted 3-tuples; this
    // matches what the WhoWroteThat gadget / Dashboard sidebar see.
    let total_present_tokens: usize = present_editors.iter().map(|e| e.token_count).sum();
    let present_editors_json: Vec<Value> = present_editors
        .iter()
        .map(|e| {
            let name = display_name(&e.editor, editor_names);
            let pct = if total_present_tokens == 0 {
                0.0
            } else {
                e.token_count as f64 * 100.0 / total_present_tokens as f64
            };
            json!([name, e.class_name, pct])
        })
        .collect();

    // Compose final envelope — order matters for byte-level
    // comparisons against captured fixtures.
    let mut env = Map::new();
    env.insert("extended_html".into(), Value::String(extended_html));
    env.insert("present_editors".into(), Value::Array(present_editors_json));
    env.insert("tokens".into(), Value::Array(tokens_json));
    env.insert("revisions".into(), Value::Object(revisions_map));
    env.insert(
        "biggest_conflict_score".into(),
        Value::Number(data.biggest_conflict_score.into()),
    );
    env.insert("success".into(), Value::Bool(true));
    env.insert("rev_id".into(), Value::Number(rev_id.into()));
    env.insert("page_title".into(), Value::String(title.to_string()));
    Value::Object(env)
}

/// On cache-miss (article not on disk), ask MW for `(title, page_id,
/// last_revid)`, spawn the same background ingest the rev_content
/// path uses, and return the "still processing" envelope.
async fn trigger_whocolor_cache_miss(
    state: AppState,
    lang: &str,
    title: &str,
    target_rev_id: Option<u64>,
) -> Response {
    let mw_arc: Arc<MwClient> = match state.mw_client(lang) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "mw client unavailable");
            return still_processing(title, target_rev_id);
        }
    };
    let info = match mw_arc.resolve_title(title).await {
        Ok(i) => i,
        Err(wikiwho_mwclient::MwError::PageMissing { .. }) => {
            tracing::debug!(lang = %lang, title = %title, "title not on MW");
            return still_processing(title, target_rev_id);
        }
        Err(e) => {
            tracing::warn!(lang = %lang, error = %e, "MW resolve_title failed");
            return still_processing(title, target_rev_id);
        }
    };
    let end_rev_id = target_rev_id.unwrap_or(info.last_revid);
    if state.try_claim_in_flight(lang, info.page_id) {
        let mw_clone = mw_arc.clone();
        let page_id = info.page_id;
        let fetcher = async move {
            let fetcher = mw_clone.fetch_revisions(page_id, end_rev_id);
            cache_miss::collect_all_revisions(fetcher)
                .await
                .map_err(Into::into)
        };
        tracing::info!(
            lang = %lang,
            page_id = info.page_id,
            end_rev_id = end_rev_id,
            "spawning whocolor cache-miss"
        );
        // `JoinHandle` is intentionally dropped — production code is
        // fire-and-forget; only tests `.await` it.
        // MW echoes titles back with spaces (`Delon Hampton`); our
        // TitleIndex keys on the underscored form that URL lookups
        // produce. Normalize before storing so the next request hits
        // the on-disk article instead of re-spawning.
        std::mem::drop(state.spawn_cache_miss(
            lang.to_string(),
            normalize_title(&info.title),
            info.page_id,
            fetcher,
        ));
    }
    still_processing(title, target_rev_id)
}

fn still_processing(title: &str, rev_id: Option<u64>) -> Response {
    let body = json!({
        "info": "Requested data is not currently available in WikiWho database. It will be available soon.",
        "success": false,
        "rev_id": rev_id_value(rev_id),
        "page_title": title,
    });
    (StatusCode::OK, Json(body)).into_response()
}

fn vandalism_response(title: &str, rev_id: u64) -> Response {
    let body = json!({
        "info": format!("Requested revision ({rev_id}) is detected as vandalism by WikiWho."),
        "success": false,
        "rev_id": rev_id,
        "page_title": title,
    });
    (StatusCode::OK, Json(body)).into_response()
}

fn error_500(err: ServerError, title: &str) -> Response {
    tracing::error!(error = %err, "whocolor server error");
    let body = json!({
        "error": err.to_string(),
        "success": false,
        "rev_id": Value::Null,
        "page_title": title,
    });
    (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
}

fn error_503(message: &str, title: &str, rev_id: Option<u64>) -> Response {
    let body = json!({
        "error": message,
        "success": false,
        "rev_id": rev_id_value(rev_id),
        "page_title": title,
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

fn rev_id_value(rev_id: Option<u64>) -> Value {
    match rev_id {
        Some(id) => Value::Number(id.into()),
        None => Value::Null,
    }
}

/// Normalize spaces to underscores in titles (axum may decode `%20`
/// before we see it). Same convention as the rev_content handler.
fn normalize_title(raw: &str) -> String {
    raw.replace(' ', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wikiwho_attribute::whocolor::{WhoColorRevision, WhoColorToken};

    #[test]
    fn unique_editor_ids_dedup_and_skip_anons() {
        let data = WhoColorData {
            tokens: vec![],
            revisions: vec![
                (
                    10,
                    WhoColorRevision {
                        timestamp: "2024-01-01T00:00:00Z".into(),
                        parent_rev_id: 0,
                        editor: "1".into(),
                    },
                ),
                (
                    20,
                    WhoColorRevision {
                        timestamp: "2024-01-02T00:00:00Z".into(),
                        parent_rev_id: 10,
                        editor: "0|192.0.2.1".into(),
                    },
                ),
                (
                    30,
                    WhoColorRevision {
                        timestamp: "2024-01-03T00:00:00Z".into(),
                        parent_rev_id: 20,
                        editor: "1".into(), // duplicate
                    },
                ),
                (
                    40,
                    WhoColorRevision {
                        timestamp: "2024-01-04T00:00:00Z".into(),
                        parent_rev_id: 30,
                        editor: "".into(), // missing editor
                    },
                ),
            ],
            biggest_conflict_score: 0,
        };
        let ids = unique_registered_editor_ids(&data);
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn build_envelope_shape_matches_api_md() {
        let mut article = Article::new("Test");
        article.page_id = Some(42);
        article.ordered_revisions = vec![10];
        article.revisions.insert(
            10,
            wikiwho_attribute::structures::Revision {
                id: 10,
                editor: "1".into(),
                timestamp: "2024-01-01T00:00:00Z".into(),
                ..Default::default()
            },
        );
        let data = WhoColorData {
            tokens: vec![WhoColorToken {
                str: "hello".into(),
                o_rev_id: 10,
                inbound: vec![],
                outbound: vec![],
                editor: "1".into(),
                conflict_score: 3,
                age_seconds: 12345.0,
            }],
            revisions: vec![(
                10,
                WhoColorRevision {
                    timestamp: "2024-01-01T00:00:00Z".into(),
                    parent_rev_id: 0,
                    editor: "1".into(),
                },
            )],
            biggest_conflict_score: 3,
        };
        let mut names = HashMap::new();
        names.insert(1u64, "Alice".to_string());

        let env = build_whocolor_envelope(
            &article,
            "Test",
            10,
            &data,
            &names,
            "<html>...</html>".into(),
            &[crate::whocolor_html::PresentEditorEntry {
                editor: "1".into(),
                class_name: "1".into(),
                token_count: 1,
            }],
        );

        // Top-level shape.
        assert_eq!(env["page_title"], Value::String("Test".into()));
        assert_eq!(env["success"], Value::Bool(true));
        assert_eq!(env["rev_id"], 10);
        assert_eq!(env["biggest_conflict_score"], 3);
        assert_eq!(env["extended_html"], "<html>...</html>");

        // tokens is array-of-arrays.
        let tokens = env["tokens"].as_array().unwrap();
        assert_eq!(tokens.len(), 1);
        let tok = tokens[0].as_array().unwrap();
        assert_eq!(tok[0], 3); // conflict_score
        assert_eq!(tok[1], "hello"); // str
        assert_eq!(tok[2], 10); // o_rev_id
        assert_eq!(tok[5], "1"); // class_name
        assert_eq!(tok[6], 12345.0); // age

        // revisions is a dict.
        let revs = env["revisions"].as_object().unwrap();
        let entry = revs["10"].as_array().unwrap();
        assert_eq!(entry[0], "2024-01-01T00:00:00Z");
        assert_eq!(entry[1], 0);
        assert_eq!(entry[2], "1"); // class_name
        assert_eq!(entry[3], "Alice"); // editor_name from `names` map

        // present_editors is array of [name, class_name, percentage].
        let pe = env["present_editors"].as_array().unwrap();
        assert_eq!(pe.len(), 1);
        let triple = pe[0].as_array().unwrap();
        assert_eq!(triple.len(), 3, "present_editors entries are triples");
        assert_eq!(triple[0], "Alice");
        assert_eq!(triple[1], "1");
        assert!((triple[2].as_f64().unwrap() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn build_envelope_anon_keeps_prefix_in_display_name() {
        // Production's `whocolor/handler.py:117` falls back to the raw
        // editor string when no MW name is known, which for anons
        // means the literal `0|<ip>` form. We mirror that.
        let article = Article::new("X");
        let data = WhoColorData {
            tokens: vec![],
            revisions: vec![(
                7,
                WhoColorRevision {
                    timestamp: "2024-01-01T00:00:00Z".into(),
                    parent_rev_id: 0,
                    editor: "0|192.0.2.1".into(),
                },
            )],
            biggest_conflict_score: 0,
        };
        let env = build_whocolor_envelope(
            &article,
            "X",
            7,
            &data,
            &HashMap::new(),
            String::new(),
            &[crate::whocolor_html::PresentEditorEntry {
                editor: "0|192.0.2.1".into(),
                class_name: token_class_name("0|192.0.2.1"),
                token_count: 1,
            }],
        );
        let revs = env["revisions"].as_object().unwrap();
        let entry = revs["7"].as_array().unwrap();
        // Class name is md5 of "0|192.0.2.1" — 32 hex chars.
        let class = entry[2].as_str().unwrap();
        assert_eq!(class.len(), 32);
        assert!(class.chars().all(|c| c.is_ascii_hexdigit()));
        // Editor name keeps the `0|` prefix — matches what the
        // production fixture emits for anons.
        assert_eq!(entry[3], "0|192.0.2.1");

        let pe = env["present_editors"].as_array().unwrap();
        let triple = pe[0].as_array().unwrap();
        assert_eq!(triple.len(), 3, "present_editors entries are triples");
        assert_eq!(triple[0], "0|192.0.2.1");
        // Percentage with one editor + one token = 100%.
        assert!((triple[2].as_f64().unwrap() - 100.0).abs() < 1e-9);
    }
}
