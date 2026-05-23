//! Query-parameter parsing matching `api/views.py:188 (get_parameters)`.
//!
//! Each opt-in field is a string compared literally against `"true"`;
//! anything else (including `"1"`, `"True"`, `""`, missing) is false.
//! The current production service does NOT lowercase the value, so
//! `?o_rev_id=True` does NOT enable the field. Mirror that quirk.

use serde::Deserialize;
use wikiwho_attribute::response::ResponseParameters;

/// Query parameters across the `rev_content` family of endpoints
/// (API.md §1-6). Each value is parsed as a string and compared to
/// `"true"` per the Python reference.
#[derive(Debug, Default, Deserialize)]
pub struct RawTokenParams {
    #[serde(default)]
    pub o_rev_id: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub token_id: Option<String>,
    #[serde(default, rename = "in")]
    pub inbound: Option<String>,
    #[serde(default, rename = "out")]
    pub outbound: Option<String>,
}

impl RawTokenParams {
    pub fn into_response_parameters(self) -> ResponseParameters {
        ResponseParameters {
            o_rev_id: as_true(self.o_rev_id.as_deref()),
            editor: as_true(self.editor.as_deref()),
            token_id: as_true(self.token_id.as_deref()),
            inbound: as_true(self.inbound.as_deref()),
            outbound: as_true(self.outbound.as_deref()),
        }
    }
}

fn as_true(raw: Option<&str>) -> bool {
    matches!(raw, Some("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(o_rev_id: &str, editor: &str, token_id: &str, inb: &str, outb: &str) -> RawTokenParams {
        RawTokenParams {
            o_rev_id: Some(o_rev_id.into()),
            editor: Some(editor.into()),
            token_id: Some(token_id.into()),
            inbound: Some(inb.into()),
            outbound: Some(outb.into()),
        }
    }

    #[test]
    fn empty_params_yield_none() {
        assert_eq!(
            RawTokenParams::default().into_response_parameters(),
            ResponseParameters::NONE
        );
    }

    #[test]
    fn all_true_yields_all() {
        let p = raw("true", "true", "true", "true", "true").into_response_parameters();
        assert_eq!(p, ResponseParameters::ALL);
    }

    #[test]
    fn non_true_values_count_as_false() {
        // Python's `get('o_rev_id', 'false') == 'true'` — capital T,
        // numeric 1, empty string, etc. all return False.
        for v in &["True", "1", "", "yes", "TRUE"] {
            let p = RawTokenParams {
                o_rev_id: Some((*v).into()),
                ..Default::default()
            }
            .into_response_parameters();
            assert!(!p.o_rev_id, "{v} should disable o_rev_id");
        }
    }

    #[test]
    fn in_and_out_map_to_inbound_outbound_in_struct() {
        let p = RawTokenParams {
            inbound: Some("true".into()),
            outbound: Some("true".into()),
            ..Default::default()
        }
        .into_response_parameters();
        assert!(p.inbound);
        assert!(p.outbound);
        assert!(!p.editor);
    }
}
