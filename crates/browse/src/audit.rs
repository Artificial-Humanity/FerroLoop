//! The UI and accessibility gate auditor behind `fl browse check`.
//!
//! It reads the same scan as the distiller, so its handles are the snapshot's handles, and it
//! skips hidden content the same way. Every check is a fact about the markup; none needs a
//! layout or a script run.
//!
//! | code | severity | what it flags |
//! |------|----------|---------------|
//! | [`MISSING_VIEWPORT_META`] | error | no `<meta name="viewport">` whose content sets `width=device-width` |
//! | [`UNLABELED_CONTROL`] | error | a visible `button`, `input` (not `type=hidden`), `textarea` or `select` with no accessible name |
//! | [`IOS_CONTACT_AUTOFILL_HAZARD`] | error | a `textarea` with `autocomplete="one-time-code"` |
//! | [`FIXED_MIN_WIDTH_OVERFLOW_HAZARD`] | warning | an inline `min-width` in px wider than the viewport |
//!
//! The first three are defects on any device. The fourth is a hazard that depends on the
//! viewport and on the container, so it warns; `--strict` turns warnings into a failing gate.

use crate::Viewport;
use crate::distill::{Scan, scan};
use crate::html::{Document, parse};
use serde::Serialize;

pub const MISSING_VIEWPORT_META: &str = "missing-viewport-meta";
pub const UNLABELED_CONTROL: &str = "unlabeled-control";
pub const IOS_CONTACT_AUTOFILL_HAZARD: &str = "ios-contact-autofill-hazard";
pub const FIXED_MIN_WIDTH_OVERFLOW_HAZARD: &str = "fixed-min-width-overflow-hazard";

/// Every code the auditor emits, with its severity.
pub const CHECKS: &[(&str, Severity)] = &[
    (MISSING_VIEWPORT_META, Severity::Error),
    (UNLABELED_CONTROL, Severity::Error),
    (IOS_CONTACT_AUTOFILL_HAZARD, Severity::Error),
    (FIXED_MIN_WIDTH_OVERFLOW_HAZARD, Severity::Warning),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One finding. `handle` is the `@eN` of the element when it is interactive.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BrowseFinding {
    pub severity: Severity,
    pub code: String,
    pub handle: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditReport {
    pub viewport: Viewport,
    pub findings: Vec<BrowseFinding>,
    pub error_count: usize,
    pub warning_count: usize,
}

impl AuditReport {
    /// Whether the gate passes: no error, and with `strict` no warning either.
    pub fn passed(&self, strict: bool) -> bool {
        self.error_count == 0 && (!strict || self.warning_count == 0)
    }
}

/// Whether a viewport meta `content` sets `width=device-width`.
fn sets_device_width(content: &str) -> bool {
    content.split([',', ';']).any(|part| {
        part.split_once('=').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("width") && v.trim().eq_ignore_ascii_case("device-width")
        })
    })
}

/// A short description of a control for a message.
fn describe(e: &crate::distill::SnapElement) -> String {
    let mut s = format!("<{}", e.tag);
    for k in ["type", "id", "name"] {
        if let Some(v) = e.attrs.get(k) {
            s.push_str(&format!(" {k}=\"{v}\""));
        }
    }
    s.push('>');
    s
}

fn audit_scan(scan: &Scan, viewport: Viewport) -> AuditReport {
    let mut findings = Vec::new();
    let mut push = |severity, code: &str, handle: Option<String>, message: String| {
        findings.push(BrowseFinding {
            severity,
            code: code.to_string(),
            handle,
            message,
        });
    };

    match scan.viewport_meta.as_deref() {
        None => push(
            Severity::Error,
            MISSING_VIEWPORT_META,
            None,
            "no <meta name=\"viewport\">; add <meta name=\"viewport\" \
             content=\"width=device-width, initial-scale=1\"> so a phone does not lay the page \
             out at desktop width"
                .to_string(),
        ),
        Some(c) if !sets_device_width(c) => push(
            Severity::Error,
            MISSING_VIEWPORT_META,
            None,
            format!(
                "the viewport meta (\"{c}\") does not set width=device-width; a phone lays the \
                 page out at its own default width"
            ),
        ),
        Some(_) => {}
    }

    for e in &scan.elements {
        if matches!(e.tag.as_str(), "button" | "input" | "textarea" | "select") && e.name.is_empty()
        {
            push(
                Severity::Error,
                UNLABELED_CONTROL,
                Some(e.handle.clone()),
                format!(
                    "{} has no accessible name; give it text, a <label>, aria-label, \
                     aria-labelledby, title or placeholder",
                    describe(e)
                ),
            );
        }
    }

    for e in &scan.elements {
        if e.tag == "textarea"
            && e.autocomplete.as_deref().is_some_and(|a| {
                a.split_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("one-time-code"))
            })
        {
            push(
                Severity::Error,
                IOS_CONTACT_AUTOFILL_HAZARD,
                Some(e.handle.clone()),
                format!(
                    "{} sets autocomplete=\"one-time-code\", which is not valid on a textarea; \
                     iOS WebKit treats it as a contact field and offers Contact AutoFill. Use an \
                     <input> for a code, or autocomplete=\"off\"",
                    describe(e)
                ),
            );
        }
    }

    for m in &scan.min_widths {
        if m.px > f64::from(viewport.width) {
            push(
                Severity::Warning,
                FIXED_MIN_WIDTH_OVERFLOW_HAZARD,
                m.handle.clone(),
                format!(
                    "<{}> has an inline min-width of {}px, wider than the {}px viewport; it \
                     overflows horizontally",
                    m.tag, m.px, viewport.width
                ),
            );
        }
    }

    let error_count = findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warning_count = findings.len() - error_count;
    AuditReport {
        viewport,
        findings,
        error_count,
        warning_count,
    }
}

/// Audit a parsed document for a viewport.
pub fn audit_document(doc: &Document, viewport: Viewport) -> AuditReport {
    audit_scan(&scan(doc), viewport)
}

/// Parse and audit `html`.
pub fn audit_html(html: &str, viewport: Viewport) -> AuditReport {
    audit_document(&parse(html), viewport)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VP: Viewport = Viewport {
        width: 320,
        height: 568,
    };
    const META: &str = "<meta name=viewport content='width=device-width, initial-scale=1'>";

    fn codes(html: &str) -> Vec<String> {
        audit_html(html, VP)
            .findings
            .into_iter()
            .map(|f| f.code)
            .collect()
    }

    fn with_meta(body: &str) -> String {
        format!("<head>{META}</head><body>{body}</body>")
    }

    #[test]
    fn a_clean_page_has_no_findings() {
        let r = audit_html(
            &with_meta("<label for=a>A</label><input id=a><button>Go</button>"),
            VP,
        );
        assert_eq!(r.findings, vec![]);
        assert!(r.passed(true));
    }

    #[test]
    fn missing_viewport_meta_positive_and_negative() {
        assert_eq!(codes("<p>x</p>"), vec![MISSING_VIEWPORT_META]);
        assert_eq!(codes(&with_meta("<p>x</p>")), Vec::<String>::new());
        // Case and separators.
        assert!(
            codes("<meta name=VIEWPORT content='Width = Device-Width;initial-scale=1'>").is_empty()
        );
    }

    #[test]
    fn a_viewport_meta_without_device_width_is_still_missing() {
        for c in [
            "width=1024",
            "initial-scale=1",
            "",
            "device-width",
            "min-width=device-width",
        ] {
            let r = audit_html(&format!("<meta name=viewport content='{c}'>"), VP);
            assert_eq!(r.error_count, 1, "content {c:?}");
            assert_eq!(r.findings[0].code, MISSING_VIEWPORT_META);
            assert!(r.findings[0].message.contains("width=device-width"));
        }
        let no_content = audit_html("<meta name=viewport>", VP);
        assert_eq!(no_content.error_count, 1);
    }

    #[test]
    fn unlabeled_controls_are_errors_with_the_handle() {
        let r = audit_html(
            &with_meta(
                "<button>ok</button><button></button><input><textarea></textarea><select></select>",
            ),
            VP,
        );
        let got: Vec<_> = r
            .findings
            .iter()
            .map(|f| (f.code.as_str(), f.handle.as_deref(), f.severity))
            .collect();
        assert_eq!(
            got,
            vec![
                (UNLABELED_CONTROL, Some("@e2"), Severity::Error),
                (UNLABELED_CONTROL, Some("@e3"), Severity::Error),
                (UNLABELED_CONTROL, Some("@e4"), Severity::Error),
                (UNLABELED_CONTROL, Some("@e5"), Severity::Error),
            ]
        );
        assert!(r.findings[1].message.contains("<input>"));
    }

    #[test]
    fn every_kind_of_label_satisfies_the_control_check() {
        let ok = with_meta(
            "<button>text</button><button aria-label=a></button>\
             <span id=s>x</span><button aria-labelledby=s></button>\
             <button title=t></button><input placeholder=p><textarea placeholder=p></textarea>\
             <label for=i>L</label><input id=i><label>W <select></select></label>\
             <input type=submit value=Go><input type=submit><input type=reset>\
             <input type=button value=v><input type=image alt=a><input type=hidden>\
             <select title=t></select><textarea aria-label=a></textarea>",
        );
        assert_eq!(codes(&ok), Vec::<String>::new());
    }

    #[test]
    fn unlabeled_button_like_inputs_and_whitespace_names_fail() {
        let r = audit_html(
            &with_meta(
                "<input type=button><input type=image><button> \n </button>\
                 <button aria-label='  '></button>",
            ),
            VP,
        );
        assert_eq!(r.error_count, 4, "{:?}", r.findings);
    }

    #[test]
    fn hidden_controls_and_non_form_elements_are_not_flagged() {
        let ok = with_meta(
            "<input type=hidden><button hidden></button><div hidden><input></div>\
             <div style='display:none'><button></button></div><button aria-hidden=true></button>\
             <a href=x></a><div role=button></div><div data-testid=x></div>",
        );
        assert_eq!(codes(&ok), Vec::<String>::new());
    }

    #[test]
    fn ios_contact_autofill_hazard_positive_and_negative() {
        let r = audit_html(
            &with_meta("<textarea aria-label=Code autocomplete='one-time-code'></textarea>"),
            VP,
        );
        assert_eq!(r.findings.len(), 1);
        let f = &r.findings[0];
        assert_eq!(f.code, IOS_CONTACT_AUTOFILL_HAZARD);
        assert_eq!(f.severity, Severity::Error);
        assert_eq!(f.handle.as_deref(), Some("@e1"));
        // Token match, any case.
        assert_eq!(
            codes(&with_meta(
                "<textarea aria-label=c autocomplete='section-a ONE-TIME-CODE'></textarea>"
            )),
            vec![IOS_CONTACT_AUTOFILL_HAZARD]
        );
        // An <input> is the right element for a code; other values are fine.
        assert!(
            codes(&with_meta(
                "<input aria-label=c autocomplete=one-time-code>"
            ))
            .is_empty()
        );
        assert!(
            codes(&with_meta(
                "<textarea aria-label=c autocomplete=off></textarea>"
            ))
            .is_empty()
        );
        assert!(
            codes(&with_meta(
                "<textarea aria-label=c autocomplete=one-time-codes></textarea>"
            ))
            .is_empty()
        );
        // A hidden textarea is not on the page.
        assert!(
            codes(&with_meta(
                "<textarea hidden autocomplete=one-time-code></textarea>"
            ))
            .is_empty()
        );
    }

    #[test]
    fn min_width_over_the_viewport_warns() {
        let r = audit_html(
            &with_meta(
                "<div style='min-width: 400px'>x</div><button style='min-width:321px'>b</button>",
            ),
            VP,
        );
        let got: Vec<_> = r
            .findings
            .iter()
            .map(|f| (f.code.as_str(), f.handle.as_deref(), f.severity))
            .collect();
        assert_eq!(
            got,
            vec![
                (FIXED_MIN_WIDTH_OVERFLOW_HAZARD, None, Severity::Warning),
                (
                    FIXED_MIN_WIDTH_OVERFLOW_HAZARD,
                    Some("@e1"),
                    Severity::Warning
                ),
            ]
        );
        assert_eq!((r.error_count, r.warning_count), (0, 2));
        assert!(r.passed(false));
        assert!(!r.passed(true));
        assert!(r.findings[0].message.contains("400px") && r.findings[0].message.contains("320px"));
    }

    #[test]
    fn min_width_at_or_under_the_viewport_or_not_in_px_is_fine() {
        let ok = with_meta(
            "<div style='min-width:320px'></div><div style='min-width:100%'></div>\
             <div style='min-width:40rem'></div><div style='min-width:auto'></div>\
             <div style='width:900px'></div><div hidden style='min-width:900px'></div>",
        );
        assert_eq!(codes(&ok), Vec::<String>::new());
    }

    #[test]
    fn min_width_follows_the_audited_viewport() {
        let html = with_meta("<div style='min-width: 400px'></div>");
        assert_eq!(audit_html(&html, VP).warning_count, 1);
        let wide = Viewport {
            width: 1280,
            height: 800,
        };
        assert_eq!(audit_html(&html, wide).warning_count, 0);
        assert_eq!(audit_html(&html, wide).viewport, wide);
    }

    #[test]
    fn counts_and_gate_semantics() {
        let r = audit_html("<button></button><div style='min-width:999px'></div>", VP);
        assert_eq!((r.error_count, r.warning_count), (2, 1));
        assert!(!r.passed(false));
        let warn_only = audit_html(&with_meta("<div style='min-width:999px'></div>"), VP);
        assert!(warn_only.passed(false) && !warn_only.passed(true));
    }

    #[test]
    fn findings_are_ordered_deterministically_and_serialize_with_the_contract_names() {
        let html = "<textarea autocomplete=one-time-code></textarea><button></button>\
                    <div style='min-width:999px'></div>";
        let a = audit_html(html, VP);
        let b = audit_html(html, VP);
        assert_eq!(a, b);
        assert_eq!(
            a.findings
                .iter()
                .map(|f| f.code.as_str())
                .collect::<Vec<_>>(),
            vec![
                MISSING_VIEWPORT_META,
                UNLABELED_CONTROL,
                UNLABELED_CONTROL,
                IOS_CONTACT_AUTOFILL_HAZARD,
                FIXED_MIN_WIDTH_OVERFLOW_HAZARD
            ]
        );
        let v = serde_json::to_value(&a.findings[1]).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "severity": "error",
                "code": "unlabeled-control",
                "handle": "@e1",
                "message": a.findings[1].message,
            })
        );
        assert!(serde_json::to_value(&a.findings[0]).unwrap()["handle"].is_null());
    }

    #[test]
    fn the_check_table_matches_what_is_emitted() {
        let html =
            "<textarea autocomplete=one-time-code></textarea><div style='min-width:999px'></div>";
        for f in audit_html(html, VP).findings {
            let sev = CHECKS.iter().find(|(c, _)| *c == f.code).unwrap().1;
            assert_eq!(f.severity, sev, "{}", f.code);
        }
    }

    #[test]
    fn malformed_markup_is_audited_without_panicking() {
        let r = audit_html(
            "<button><textarea autocomplete=one-time-code <div style=min-width:9999px",
            VP,
        );
        assert!(r.error_count >= 1);
    }
}
