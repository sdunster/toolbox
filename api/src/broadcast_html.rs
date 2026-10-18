//! HTML broadcast bodies — pure processing, no I/O.
//!
//! An owner designs a rich email somewhere else (often by asking Claude for
//! it), then pastes the HTML into Toolbox or uploads an export `.zip`
//! (`index.html` plus an images folder). See CLAUDE.md's "Broadcasts" entry.
//! This module turns that HTML into what is actually sent:
//!
//! - **Images are hosted by Toolbox.** A relative image reference
//!   (`images/logo.png`, `./hero.jpg`) is rewritten to the public URL the
//!   owner uploaded that file to (`createBroadcastAssetUpload`), looked up in
//!   the caller's `asset_urls` map. One with no upload yet is kept as-is and
//!   reported by [`missing_assets`], and a broadcast with any is refused at
//!   send time. Absolute `https://` references pass through untouched.
//!   Covered: `<img src>`, the `background` attribute, and `url(…)` in a
//!   `style` attribute or `<style>` block.
//! - **`data:` and `cid:` images are refused**, with a message saying to
//!   upload the file instead: Gmail and Outlook strip `data:` images, and a
//!   `cid:` reference points at a MIME part this sender never attaches.
//! - **Anything that runs code is stripped**: `<script>`, frames, plugins,
//!   forms, `<base>`, `<meta http-equiv=refresh>`, `on*` handlers, and
//!   `javascript:`/`vbscript:` URLs. Mail clients strip these anyway; doing
//!   it here keeps the stored HTML honest about what recipients get, and is
//!   defence in depth behind the web app's sandboxed preview.
//!
//! Processing is idempotent — running it again over its own output with an
//! empty map changes nothing — so the processed HTML is what gets stored,
//! and [`missing_assets`] can be recomputed from it at any time.
//!
//! [`html_to_text`] derives the `text/plain` alternative when the owner
//! doesn't write one.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

use anyhow::{Result, anyhow, bail};
use lol_html::html_content::ContentType;
use lol_html::{RewriteStrSettings, element, rewrite_str, text};

/// Most bytes an HTML body may be. Gmail clips a message past ~102 KB
/// (hiding the rest behind "View entire message"), and the body lives on
/// the `broadcast` row, which also holds every recipient's status.
pub const MAX_HTML_BYTES: usize = 100_000;

/// Elements removed outright, content and all.
const REMOVED_ELEMENTS: &str = "script, iframe, frame, frameset, object, embed, applet, \
     form, input, button, select, textarea, base, noscript";

/// The result of [`process`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub html: String,
    /// Relative image references with no uploaded file yet, deduplicated and
    /// sorted.
    pub missing_assets: Vec<String>,
}

/// Sanitize `html` and rewrite its relative image references through
/// `asset_urls` (relative path → public URL) — see the module doc. Errors
/// are user-facing messages.
pub fn process(html: &str, asset_urls: &HashMap<String, String>) -> Result<Processed> {
    if html.len() > MAX_HTML_BYTES {
        bail!(
            "the HTML is too large ({} KB; at most {} KB)",
            html.len().div_ceil(1000),
            MAX_HTML_BYTES / 1000
        );
    }
    let mut map: HashMap<String, String> = HashMap::new();
    for (path, url) in asset_urls {
        check_hosted_url(url)?;
        map.insert(normalize_asset_path(path), url.clone());
    }

    let state = RefCell::new(State {
        map: &map,
        missing: BTreeSet::new(),
        refused: BTreeSet::new(),
        css: String::new(),
    });
    let out = rewrite_str(
        html,
        RewriteStrSettings::new()
            .append_element_content_handler(element!(REMOVED_ELEMENTS, |el| {
                el.remove();
                Ok(())
            }))
            .append_element_content_handler(element!("meta[http-equiv]", |el| {
                let refresh = el
                    .get_attribute("http-equiv")
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("refresh"));
                if refresh {
                    el.remove();
                }
                Ok(())
            }))
            .append_element_content_handler(element!("*", |el| {
                let mut st = state.borrow_mut();
                let names: Vec<String> = el.attributes().iter().map(|a| a.name()).collect();
                for name in names {
                    let Some(value) = el.get_attribute(&name) else {
                        continue;
                    };
                    if name.starts_with("on") || is_script_url(&value) {
                        el.remove_attribute(&name);
                    } else if name == "srcset" {
                        // Rare in email, and each candidate would need
                        // hosting too: `src` is what clients use.
                        el.remove_attribute(&name);
                    } else if name == "background" || (name == "src" && el.tag_name() == "img") {
                        if let Some(new) = st.image_ref(&value) {
                            el.set_attribute(&name, &new)?;
                        }
                    } else if name == "style" {
                        let new = st.rewrite_css(&value);
                        if new != value {
                            el.set_attribute(&name, &new)?;
                        }
                    }
                }
                Ok(())
            }))
            .append_element_content_handler(text!("style", |t| {
                let mut st = state.borrow_mut();
                st.css.push_str(t.as_str());
                if t.last_in_text_node() {
                    let css = std::mem::take(&mut st.css);
                    let new = st.rewrite_css(&css);
                    // Raw text: a `<style>` body is never entity-decoded,
                    // so escaping it (`ContentType::Text`) would corrupt
                    // selectors like `a > b`.
                    t.replace(&new, ContentType::Html);
                } else {
                    t.remove();
                }
                Ok(())
            })),
    )
    .map_err(|e| anyhow!("could not read the HTML: {e}"))?;

    let st = state.into_inner();
    if !st.refused.is_empty() {
        bail!(
            "{} — upload the image file instead",
            st.refused.into_iter().collect::<Vec<_>>().join("; ")
        );
    }
    if out.trim().is_empty() {
        bail!("the HTML is empty");
    }
    Ok(Processed {
        html: out,
        missing_assets: st.missing.into_iter().collect(),
    })
}

/// Relative image references in already-processed `html` that still have no
/// uploaded file. Empty if the HTML can't be read (it was processed once
/// already, so that would be a bug, not a missing image).
pub fn missing_assets(html: &str) -> Vec<String> {
    process(html, &HashMap::new())
        .map(|p| p.missing_assets)
        .unwrap_or_default()
}

/// The key an asset path is matched on: `./images/a.png`, `images/a.png`
/// and `/images/a.png` are all `images/a.png`.
pub fn normalize_asset_path(path: &str) -> String {
    let mut p = path.trim().replace("&amp;", "&").replace('\\', "/");
    loop {
        if let Some(rest) = p.strip_prefix("./") {
            p = rest.to_string();
        } else if let Some(rest) = p.strip_prefix('/') {
            p = rest.to_string();
        } else {
            break;
        }
    }
    p
}

/// A URL an asset map may point at: absolute `https://` (or plain
/// `http://localhost`, for local development), and free of anything that
/// could break out of a quoted attribute or a CSS `url('…')`.
fn check_hosted_url(url: &str) -> Result<()> {
    let ok = (url.starts_with("https://")
        || url.starts_with("http://localhost:")
        || url.starts_with("http://localhost/"))
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "'\"()<>\\".contains(c));
    if !ok {
        bail!("{url:?} is not a usable image URL");
    }
    Ok(())
}

struct State<'a> {
    map: &'a HashMap<String, String>,
    missing: BTreeSet<String>,
    refused: BTreeSet<String>,
    /// A `<style>` block's text, accumulated across chunks.
    css: String,
}

impl State<'_> {
    /// What an image reference should become: `Some(new)` to rewrite it,
    /// `None` to leave it alone (absolute, missing, or refused — the last
    /// two are recorded).
    fn image_ref(&mut self, raw: &str) -> Option<String> {
        let value = raw.trim();
        let lower = value.to_ascii_lowercase();
        if value.is_empty() || lower.starts_with("https://") || lower.starts_with("http://") {
            return None;
        }
        if let Some(rest) = value.strip_prefix("//") {
            return Some(format!("https://{rest}"));
        }
        if lower.starts_with("data:") {
            self.refused
                .insert("an image is embedded as a data: URL".to_string());
            return None;
        }
        if lower.starts_with("cid:") {
            self.refused.insert(format!(
                "{value:?} refers to an attachment this email won't have"
            ));
            return None;
        }
        if lower.starts_with('#') || lower.contains(':') {
            // Some other scheme (or a fragment): nothing to host.
            return None;
        }
        let key = normalize_asset_path(value);
        match self.map.get(&key) {
            Some(url) => Some(url.clone()),
            None => {
                self.missing.insert(key);
                None
            }
        }
    }

    /// Rewrite every `url(…)` in a CSS string through [`Self::image_ref`].
    fn rewrite_css(&mut self, css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(pos) = find_ascii_ci(rest, "url(") {
            let (before, after) = rest.split_at(pos + 4);
            out.push_str(before);
            let Some(end) = after.find(')') else {
                rest = after;
                break;
            };
            let inner = &after[..end];
            let unquoted = inner.trim().trim_matches(|c| c == '\'' || c == '"');
            match self.image_ref(unquoted) {
                Some(url) => out.push_str(&format!("'{url}'")),
                None => out.push_str(inner),
            }
            out.push(')');
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        out
    }
}

fn is_script_url(value: &str) -> bool {
    // Browsers ignore whitespace and control characters inside a scheme.
    let scheme: String = value
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control())
        .take(11)
        .collect::<String>()
        .to_ascii_lowercase();
    scheme.starts_with("javascript:") || scheme.starts_with("vbscript:")
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

// ── plain-text alternative ───────────────────────────────────────────────

/// Elements whose content is never visible text.
const HIDDEN: &[&str] = &["head", "style", "script", "title", "noscript", "template"];
/// Elements that end a line.
const LINE: &[&str] = &[
    "br", "div", "tr", "li", "dt", "dd", "section", "header", "footer", "center", "address",
];
/// Elements that end a paragraph (a blank line).
const PARAGRAPH: &[&str] = &[
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "table",
    "ul",
    "ol",
    "dl",
    "blockquote",
    "hr",
    "pre",
];

/// A readable plain-text rendering of `html` — the `text/plain` part of an
/// HTML broadcast whose owner didn't write one. Block elements become line
/// breaks, and a link becomes `text (url)` unless its text already is the
/// URL. Good enough for a text-only mail client, not a faithful layout.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut hidden: Vec<String> = Vec::new();
    // Open links: the href and where its text began in `out`.
    let mut links: Vec<(Option<String>, usize)> = Vec::new();
    let mut pending_space = false;

    let mut rest = html;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("<!--") {
            rest = after.find("-->").map_or("", |end| &after[end + 3..]);
            continue;
        }
        if rest.starts_with('<') {
            let end = tag_end(rest);
            let tag = &rest[1..end.saturating_sub(1).max(1)];
            rest = &rest[end..];
            let closing = tag.starts_with('/');
            let body = tag.trim_start_matches('/');
            let name: String = body
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if name.is_empty() {
                continue;
            }
            if HIDDEN.contains(&name.as_str()) {
                if closing {
                    if let Some(i) = hidden.iter().rposition(|h| *h == name) {
                        hidden.truncate(i);
                    }
                } else if !body.trim_end().ends_with('/') {
                    hidden.push(name);
                }
                continue;
            }
            if !hidden.is_empty() {
                continue;
            }
            match (name.as_str(), closing) {
                ("a", false) => links.push((attr(body, "href"), out.len())),
                ("a", true) => {
                    if let Some((Some(href), start)) = links.pop() {
                        let label = out[start..].trim();
                        let href_shown = href.trim_start_matches("mailto:");
                        let worth_showing = href.starts_with("http") || href.starts_with("mailto:");
                        if worth_showing && label != href && label != href_shown {
                            if label.is_empty() {
                                out.push_str(href_shown);
                            } else {
                                out.push_str(&format!(" ({href_shown})"));
                            }
                        }
                    }
                }
                ("li", false) => {
                    newline(&mut out);
                    out.push_str("- ");
                    pending_space = false;
                }
                ("td" | "th", true) => pending_space = true,
                (n, _) if PARAGRAPH.contains(&n) => {
                    newline(&mut out);
                    if closing || n == "hr" {
                        out.push('\n');
                    }
                    pending_space = false;
                }
                (n, _) if LINE.contains(&n) => {
                    newline(&mut out);
                    pending_space = false;
                }
                _ => {}
            }
            continue;
        }
        let next = rest.find('<').unwrap_or(rest.len());
        let (text, after) = rest.split_at(next);
        rest = after;
        if !hidden.is_empty() {
            continue;
        }
        for word_or_space in split_whitespace_keep(&decode_entities(text)) {
            match word_or_space {
                None => pending_space = true,
                Some(word) => {
                    if pending_space && !out.is_empty() && !out.ends_with(['\n', ' ']) {
                        out.push(' ');
                    }
                    pending_space = false;
                    out.push_str(&word);
                }
            }
        }
    }

    // Tidy: trim every line, and no more than one blank line in a row.
    let mut tidy = String::new();
    let mut blank_run = 0;
    for line in out.lines().map(str::trim) {
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 || tidy.is_empty() {
                continue;
            }
        } else {
            blank_run = 0;
        }
        tidy.push_str(line);
        tidy.push('\n');
    }
    tidy.trim_end().to_string()
}

/// The index just past a tag starting at `s[0] == '<'`, honouring quoted
/// attribute values (which may contain `>`).
fn tag_end(s: &str) -> usize {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => return i + 1,
            _ => {}
        }
    }
    s.len()
}

/// A tag body's attribute value, entity-decoded (`a href="…"`).
fn attr(tag_body: &str, name: &str) -> Option<String> {
    let lower = tag_body.to_ascii_lowercase();
    let mut from = 0;
    while let Some(pos) = lower[from..].find(name) {
        let start = from + pos;
        from = start + name.len();
        let preceded = lower[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
        let rest = tag_body[from..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        if !preceded {
            continue;
        }
        let rest = rest.trim_start();
        let value = match rest.chars().next() {
            Some(q @ ('"' | '\'')) => rest[1..].split(q).next().unwrap_or(""),
            _ => rest
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or(""),
        };
        return Some(decode_entities(value.trim()));
    }
    None
}

fn newline(out: &mut String) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Split text into words (`Some`) and runs of whitespace (`None`).
fn split_whitespace_keep(text: &str) -> Vec<Option<String>> {
    let mut parts = Vec::new();
    let mut word = String::new();
    for c in text.chars() {
        // A non-breaking space is a real space in a text-only client.
        if c.is_whitespace() && c != '\u{a0}' {
            if !word.is_empty() {
                parts.push(Some(std::mem::take(&mut word)));
            }
            if parts.last() != Some(&None) {
                parts.push(None);
            }
        } else {
            word.push(if c == '\u{a0}' { ' ' } else { c });
        }
    }
    if !word.is_empty() {
        parts.push(Some(word));
    }
    parts
}

/// Decode the entities email HTML actually uses: the XML five, `&nbsp;`, a
/// few typographic ones, and numeric references. Anything else is kept
/// verbatim.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let Some(end) = rest[1..].find(';').map(|e| e + 1).filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let name = &rest[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "rsquo" => Some('’'),
            "lsquo" => Some('‘'),
            "rdquo" => Some('”'),
            "ldquo" => Some('“'),
            "copy" => Some('©'),
            "zwnj" | "zwj" => Some('\u{200c}'),
            _ => name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some('\u{200c}') => {}
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const HOSTED: &str = "https://app.example.com/email-assets/i1/abc/logo.png";

    #[test]
    fn a_relative_image_is_rewritten_through_the_asset_map() {
        let p = process(
            r#"<p><img src="./images/logo.png" alt="Logo"></p>"#,
            &map(&[("images/logo.png", HOSTED)]),
        )
        .unwrap();
        assert!(p.html.contains(&format!(r#"src="{HOSTED}""#)), "{}", p.html);
        assert!(p.missing_assets.is_empty());
    }

    #[test]
    fn an_unmapped_relative_image_is_kept_and_reported_missing() {
        let p = process(
            r#"<img src="images/a.png"><img src="/images/a.png"><td background="bg.jpg">"#,
            &HashMap::new(),
        )
        .unwrap();
        assert!(p.html.contains(r#"src="images/a.png""#), "{}", p.html);
        assert_eq!(p.missing_assets, vec!["bg.jpg", "images/a.png"]);
    }

    #[test]
    fn absolute_images_pass_through_and_protocol_relative_ones_become_https() {
        let p = process(
            r#"<img src="https://cdn.example.com/a.png"><img src="//cdn.example.com/b.png">"#,
            &HashMap::new(),
        )
        .unwrap();
        assert!(p.html.contains(r#"src="https://cdn.example.com/a.png""#));
        assert!(p.html.contains(r#"src="https://cdn.example.com/b.png""#));
        assert!(p.missing_assets.is_empty());
    }

    #[test]
    fn css_urls_in_style_attributes_and_blocks_are_rewritten() {
        let p = process(
            r#"<style>.hero { background: url("hero.jpg") } a > b { color: red }</style>
               <div style="background-image:url(hero.jpg)">x</div>"#,
            &map(&[("hero.jpg", HOSTED)]),
        )
        .unwrap();
        assert!(p.html.contains(&format!("url('{HOSTED}')")), "{}", p.html);
        assert!(!p.html.contains("hero.jpg\""), "{}", p.html);
        // A `<style>` body is raw text: not entity-escaped.
        assert!(p.html.contains("a > b"), "{}", p.html);
    }

    #[test]
    fn scripts_handlers_and_javascript_urls_are_stripped() {
        let p = process(
            r#"<script>alert(1)</script><a href=" javascript:alert(1)" onclick="x()">hi</a>
               <iframe src="https://evil.example"></iframe><form><input></form>
               <meta http-equiv="refresh" content="0;url=https://evil.example">
               <img src="https://cdn.example.com/a.png" onerror="x()">"#,
            &HashMap::new(),
        )
        .unwrap();
        for bad in [
            "<script",
            "alert(1)",
            "javascript",
            "onclick",
            "onerror",
            "<iframe",
            "<form",
            "<input",
            "refresh",
        ] {
            assert!(!p.html.contains(bad), "{bad} survived: {}", p.html);
        }
        assert!(p.html.contains(">hi</a>"), "{}", p.html);
    }

    #[test]
    fn data_and_cid_images_are_refused() {
        let e = process(r#"<img src="data:image/png;base64,AAAA">"#, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("data:") && e.contains("upload"), "{e}");
        let e = process(r#"<img src="cid:logo@x">"#, &HashMap::new())
            .unwrap_err()
            .to_string();
        assert!(e.contains("cid:logo@x"), "{e}");
    }

    #[test]
    fn processing_is_idempotent() {
        let once = process(
            r#"<style>p{background:url(a.png)}</style><img src="b.png"><img src="c.png">"#,
            &map(&[("a.png", HOSTED), ("b.png", HOSTED)]),
        )
        .unwrap();
        let twice = process(&once.html, &HashMap::new()).unwrap();
        assert_eq!(once, twice);
        assert_eq!(missing_assets(&once.html), vec!["c.png"]);
    }

    #[test]
    fn an_asset_url_must_be_plain_https() {
        for bad in [
            "http://x.example/a.png",
            "javascript:alert(1)",
            "https://x.example/a'.png",
            "https://x.example/a).png",
        ] {
            assert!(
                process("<img src=a.png>", &map(&[("a.png", bad)])).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn oversized_or_empty_html_is_refused() {
        let big = "x".repeat(MAX_HTML_BYTES + 1);
        assert!(process(&big, &HashMap::new()).is_err());
        assert!(process("   ", &HashMap::new()).is_err());
    }

    #[test]
    fn mso_conditional_comments_survive() {
        let html = "<!--[if mso]><table><tr><td><![endif]--><p>Hi</p>";
        assert!(
            process(html, &HashMap::new())
                .unwrap()
                .html
                .contains("[if mso]")
        );
    }

    #[test]
    fn text_alternative_reads_like_the_email() {
        let text = html_to_text(
            r#"<!DOCTYPE html><html><head><title>T</title><style>p{color:red}</style></head>
            <body><!-- hidden -->
              <h1>Big   news</h1>
              <p>We&rsquo;ve shipped <b>something</b>.<br>See below&nbsp;&amp; enjoy.</p>
              <table><tr><td>Left</td><td>Right</td></tr></table>
              <ul><li>One</li><li>Two</li></ul>
              <p><a href="https://example.com/x">Read more</a>
                 <a href="https://example.com/y">https://example.com/y</a>
                 <a href="mailto:hi@example.com">hi@example.com</a></p>
            </body></html>"#,
        );
        assert_eq!(
            text,
            "Big news\n\n\
             We’ve shipped something.\nSee below & enjoy.\n\n\
             Left Right\n\n\
             - One\n- Two\n\n\
             Read more (https://example.com/x) https://example.com/y hi@example.com"
        );
    }

    #[test]
    fn text_alternative_shows_the_url_of_an_image_only_link() {
        assert_eq!(
            html_to_text(r#"<a href="https://example.com"><img src="x.png"></a>"#),
            "https://example.com"
        );
    }

    #[test]
    fn entities_decode_and_unknown_ones_survive() {
        assert_eq!(decode_entities("&#65;&#x42;&amp;&bogus;&"), "AB&&bogus;&");
    }
}
