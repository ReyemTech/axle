//! Agent-written markdown, rendered safely for the web UI.
//!
//! CommonMark plus tables, strikethrough and task lists, through `pulldown-cmark` (a linear
//! pull parser). The output is safe to embed in a page because the renderer, not the author,
//! decides every tag:
//!
//! - Raw HTML is never passed through: block and inline HTML become escaped text.
//! - Links keep only `http:`, `https:` and `mailto:` destinations and open in a new tab with
//!   `rel="noopener noreferrer nofollow"`; any other link is rendered as its text alone.
//! - Images are never loaded (CSP and privacy): an image becomes a link to its address,
//!   labelled with its alt text.
//! - Headings are demoted two levels (`#` renders as `<h3>`) so peer text never competes
//!   with the page's own headings, and carry no author-chosen ids. A long document gets a
//!   collapsible "Contents" list whose anchors are namespaced by the caller.
//! - Bare `http(s)://` addresses in text become links (same rules as above).

use pulldown_cmark::{CowStr, Event, HeadingLevel, Options, Parser, Tag, TagEnd, html};

/// A document with more headings than this, or more lines than [`LONG_LINES`], gets a
/// "Contents" list.
const LONG_HEADINGS: usize = 4;
const LONG_LINES: usize = 60;

const LINK_REL: &str = "noopener noreferrer nofollow";

/// Renders `src` to HTML. `anchors` namespaces heading ids (only set when a contents list is
/// built); pass `None` where the body is shown clipped and needs no contents list.
pub fn render(src: &str, anchors: Option<&str>) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let events: Vec<Event<'_>> = Parser::new_ext(src, opts).collect();

    // Headings, for the contents list: (demoted level, text).
    let mut headings: Vec<(u8, String)> = Vec::new();
    let mut in_heading = false;
    for e in &events {
        match e {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                headings.push((demote(*level) as u8, String::new()));
            }
            Event::End(TagEnd::Heading(_)) => in_heading = false,
            Event::Text(t) | Event::Code(t) | Event::InlineHtml(t) if in_heading => {
                if let Some(h) = headings.last_mut() {
                    h.1.push_str(t);
                }
            }
            _ => {}
        }
    }
    let long = headings.len() > LONG_HEADINGS || src.lines().count() > LONG_LINES;
    let ns = anchors
        .filter(|_| long && headings.len() >= 2)
        .map(namespace);

    let mut out = String::with_capacity(src.len() + src.len() / 2);
    if let Some(ns) = &ns {
        out.push_str("<details class=\"md-toc\"><summary>Contents</summary><ol>");
        for (i, (level, text)) in headings.iter().enumerate() {
            out.push_str(&format!(
                "<li class=\"md-toc-{level}\"><a href=\"#{ns}-{}\">",
                i + 1
            ));
            out.push_str(&escape(text.trim()));
            out.push_str("</a></li>");
        }
        out.push_str("</ol></details>");
    }

    let mut safe: Vec<Event<'_>> = Vec::with_capacity(events.len());
    // For each open link: whether it was rendered as an `<a>`.
    let mut links: Vec<bool> = Vec::new();
    // An open image: its address (if allowed) and the alt text gathered so far.
    let mut image: Option<(Option<String>, String)> = None;
    let mut in_code_block = false;
    let mut heading_no = 0usize;
    for e in events {
        if let Some((_, alt)) = image.as_mut() {
            match e {
                Event::End(TagEnd::Image) => {
                    let (href, alt) = image.take().unwrap_or_default();
                    let alt = alt.trim().to_string();
                    match href {
                        Some(href) => {
                            let label = if alt.is_empty() { href.clone() } else { alt };
                            safe.push(Event::Html(
                                format!(
                                    "<a class=\"md-image\" href=\"{}\" rel=\"{LINK_REL}\" \
                                     target=\"_blank\" title=\"Image, not loaded\">",
                                    escape(&href)
                                )
                                .into(),
                            ));
                            safe.push(Event::Text(format!("image: {label}").into()));
                            safe.push(Event::Html("</a>".into()));
                        }
                        None if !alt.is_empty() => {
                            safe.push(Event::Text(format!("image: {alt}").into()))
                        }
                        None => {}
                    }
                }
                Event::Text(t) | Event::Code(t) => alt.push_str(&t),
                _ => {}
            }
            continue;
        }
        match e {
            // Raw HTML from the author is shown as text, never interpreted.
            Event::Html(t) | Event::InlineHtml(t) => safe.push(Event::Text(t)),
            Event::Start(Tag::HtmlBlock) => safe.push(Event::Start(Tag::Paragraph)),
            Event::End(TagEnd::HtmlBlock) => safe.push(Event::End(TagEnd::Paragraph)),
            Event::Start(Tag::Heading { level, .. }) => {
                heading_no += 1;
                safe.push(Event::Start(Tag::Heading {
                    level: demote(level),
                    id: ns
                        .as_ref()
                        .map(|ns| CowStr::from(format!("{ns}-{heading_no}"))),
                    classes: Vec::new(),
                    attrs: Vec::new(),
                }));
            }
            Event::End(TagEnd::Heading(level)) => {
                safe.push(Event::End(TagEnd::Heading(demote(level))))
            }
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => {
                if allowed(&dest_url) {
                    let title = if title.is_empty() {
                        String::new()
                    } else {
                        format!(" title=\"{}\"", escape(&title))
                    };
                    safe.push(Event::Html(
                        format!(
                            "<a href=\"{}\"{title} rel=\"{LINK_REL}\" target=\"_blank\">",
                            escape(dest_url.trim())
                        )
                        .into(),
                    ));
                    links.push(true);
                } else {
                    links.push(false);
                }
            }
            Event::End(TagEnd::Link) => {
                if links.pop().unwrap_or(false) {
                    safe.push(Event::Html("</a>".into()));
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                let href = allowed(&dest_url).then(|| dest_url.trim().to_string());
                image = Some((href, String::new()));
            }
            Event::Start(Tag::CodeBlock(k)) => {
                in_code_block = true;
                safe.push(Event::Start(Tag::CodeBlock(k)));
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code_block = false;
                safe.push(Event::End(TagEnd::CodeBlock));
            }
            Event::Text(t) if !in_code_block && links.is_empty() => linkify(t, &mut safe),
            other => safe.push(other),
        }
    }
    html::push_html(&mut out, safe.into_iter());
    out
}

/// Headings in peer text sit two levels below their source: `#` → `<h3>`, capped at `<h6>`.
fn demote(level: HeadingLevel) -> HeadingLevel {
    match level {
        HeadingLevel::H1 => HeadingLevel::H3,
        HeadingLevel::H2 => HeadingLevel::H4,
        HeadingLevel::H3 => HeadingLevel::H5,
        _ => HeadingLevel::H6,
    }
}

/// Only web and mail links are followed.
fn allowed(url: &str) -> bool {
    let u = url.trim().to_ascii_lowercase();
    u.starts_with("https://") || u.starts_with("http://") || u.starts_with("mailto:")
}

/// The caller's anchor prefix, reduced to `[a-z0-9-]`.
fn namespace(raw: &str) -> String {
    let ns: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("md-{ns}")
}

/// Escapes text for an HTML attribute or element.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Splits a text run so bare `http(s)://` addresses become links.
fn linkify<'a>(text: CowStr<'a>, out: &mut Vec<Event<'a>>) {
    let s: &str = &text;
    let mut rest = s;
    let mut found = false;
    while let Some(start) = find_url(rest) {
        found = true;
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
            .unwrap_or(tail.len());
        // Trailing punctuation belongs to the sentence, not the address.
        while end > 0 {
            let last = tail[..end].chars().next_back().unwrap_or(' ');
            let unbalanced_paren =
                last == ')' && tail[..end].matches('(').count() < tail[..end].matches(')').count();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']') || unbalanced_paren {
                end -= last.len_utf8();
            } else {
                break;
            }
        }
        let url = &tail[..end];
        if url.len() <= "https://".len() {
            // Just a scheme: keep it as text and look past it.
            let skip = start + "http".len();
            out.push(Event::Text(rest[..skip].to_string().into()));
            rest = &rest[skip..];
            continue;
        }
        if start > 0 {
            out.push(Event::Text(rest[..start].to_string().into()));
        }
        out.push(Event::Html(
            format!(
                "<a href=\"{}\" rel=\"{LINK_REL}\" target=\"_blank\">",
                escape(url)
            )
            .into(),
        ));
        out.push(Event::Text(url.to_string().into()));
        out.push(Event::Html("</a>".into()));
        rest = &tail[end..];
    }
    if !found {
        out.push(Event::Text(text));
    } else if !rest.is_empty() {
        out.push(Event::Text(rest.to_string().into()));
    }
}

/// The byte offset of the next `http://` or `https://` that starts a word.
fn find_url(s: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = s[from..].find("http") {
        let at = from + i;
        let t = &s[at..];
        let starts_word = s[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if starts_word && (t.starts_with("https://") || t.starts_with("http://")) {
            return Some(at);
        }
        from = at + 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::render;

    fn r(s: &str) -> String {
        render(s, Some("t"))
    }

    #[test]
    fn script_is_escaped() {
        let out = r("<script>alert(1)</script>\n\nhi <b onclick=x>b</b>");
        assert!(!out.contains("<script"), "{out}");
        assert!(!out.contains("<b "), "{out}");
        assert!(
            out.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{out}"
        );
        assert!(out.contains("&lt;b onclick=x&gt;"), "{out}");
    }

    #[test]
    fn javascript_link_is_not_a_link() {
        for src in [
            "[x](javascript:alert(1))",
            "[x]( JavaScript:alert(1))",
            "[x](data:text/html,hi)",
            "[x](/ui/tokens)",
            "<javascript:alert(1)>",
        ] {
            let out = r(src);
            assert!(!out.contains("<a"), "{src}: {out}");
        }
        assert_eq!(r("[x](javascript:alert(1))"), "<p>x</p>\n");
    }

    #[test]
    fn links_get_rel_and_target() {
        let out = r("[docs](https://example.com/a?b=1&c=\"2\") and [m](mailto:a@b.c)");
        assert!(
            out.contains(
                "<a href=\"https://example.com/a?b=1&amp;c=&quot;2&quot;\" \
                 rel=\"noopener noreferrer nofollow\" target=\"_blank\">docs</a>"
            ),
            "{out}"
        );
        assert!(out.contains("href=\"mailto:a@b.c\""), "{out}");
    }

    #[test]
    fn images_are_never_loaded() {
        let out = r("![a chart](http://evil/x.png) ![](javascript:x) ![alt](ftp://x)");
        assert!(!out.contains("<img"), "{out}");
        assert!(
            out.contains("<a class=\"md-image\" href=\"http://evil/x.png\""),
            "{out}"
        );
        assert!(out.contains("image: a chart"), "{out}");
        assert!(!out.contains("javascript"), "{out}");
        assert!(out.contains("image: alt"), "{out}");
        assert!(!out.contains("ftp://"), "{out}");
    }

    #[test]
    fn headings_are_demoted_without_ids() {
        let out = r("# One {#top}\n\n## Two\n\n### Three\n\n#### Four");
        assert!(out.contains("<h3>One {#top}</h3>"), "{out}");
        assert!(out.contains("<h4>Two</h4>"), "{out}");
        assert!(out.contains("<h5>Three</h5>"), "{out}");
        assert!(out.contains("<h6>Four</h6>"), "{out}");
        assert!(!out.contains("<h1") && !out.contains("<h2"), "{out}");
        assert!(!out.contains("id="), "{out}");
        assert!(!out.contains("md-toc"), "{out}");
    }

    #[test]
    fn table_renders() {
        let out = r("| a | b |\n|---|---|\n| 1 | ~~2~~ |\n\n- [x] done\n- [ ] todo");
        assert!(out.contains("<table>"), "{out}");
        assert!(out.contains("<th>a</th>"), "{out}");
        assert!(out.contains("<td><del>2</del></td>"), "{out}");
        assert!(out.contains("type=\"checkbox\""), "{out}");
    }

    #[test]
    fn autolinks() {
        let out = r("see <https://example.com/x> or https://axle.dev/docs. (also http://a.b/c)");
        assert!(
            out.contains("<a href=\"https://example.com/x\" rel=\"noopener noreferrer nofollow\""),
            "{out}"
        );
        assert!(out.contains(">https://axle.dev/docs</a>."), "{out}");
        assert!(out.contains(">http://a.b/c</a>)"), "{out}");
        // Not inside code.
        let code = r("`https://x.y` and\n\n```\nhttps://x.y\n```");
        assert!(!code.contains("<a"), "{code}");
    }

    #[test]
    fn long_document_gets_contents() {
        let src = "# A\n\ntext\n\n## B\n\n## C\n\n## D\n\n## E <x>\n";
        let out = render(src, Some("plan 42"));
        assert!(
            out.starts_with("<details class=\"md-toc\"><summary>Contents</summary>"),
            "{out}"
        );
        assert!(out.contains("<a href=\"#md-plan-42-1\">A</a>"), "{out}");
        assert!(out.contains("<h3 id=\"md-plan-42-1\">A</h3>"), "{out}");
        assert!(
            out.contains("<h4 id=\"md-plan-42-5\">E &lt;x&gt;</h4>"),
            "{out}"
        );
        assert!(out.contains(">E &lt;x&gt;</a>"), "{out}");
        // Short documents and clipped renders get none.
        assert!(!render("# A\n\n## B", Some("p")).contains("md-toc"));
        assert!(!render(src, None).contains("md-toc"));
        assert!(!render(src, None).contains("id="));
        // Many lines count too.
        let lines = format!("# A\n\n## B\n\n{}", "line\n".repeat(70));
        assert!(render(&lines, Some("p")).contains("md-toc"));
    }

    #[test]
    fn nested_emphasis() {
        let out = r("***bold italic*** and **bold _italic_ `code`**");
        assert!(
            out.contains("<em><strong>bold italic</strong></em>"),
            "{out}"
        );
        assert!(
            out.contains("<strong>bold <em>italic</em> <code>code</code></strong>"),
            "{out}"
        );
    }

    #[test]
    fn code_blocks_escape() {
        let out = r("```html\n<script>x</script>\n```");
        assert!(
            out.contains("<pre><code class=\"language-html\">&lt;script&gt;"),
            "{out}"
        );
    }

    #[test]
    fn hostile_inputs_stay_inert() {
        let out = r(
            "[a](https://x.y \"t\\\" onmouseover=\\\"alert(1)\") <a href=x>y</a> \
                     [<img src=x>](https://x.y)",
        );
        assert!(!out.contains("<img"), "{out}");
        assert!(!out.contains("<a href=x"), "{out}");
        assert!(!out.contains("\" onmouseover"), "{out}");
    }
}
