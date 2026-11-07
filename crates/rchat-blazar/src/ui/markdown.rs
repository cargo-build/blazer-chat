//! Message text → renderable spans: Rocket.Chat markdown (`*bold*`,
//! `_italic_`, `` `code` ``, `~strike~`, links, `@mentions`) plus
//! `:custom_emoji:` tokens resolved through the caller's emoji cache.
//!
//! Single-level by design (no nested formatting): the original client's
//! messages are overwhelmingly single-token runs, and a flat scan keeps the
//! parser testable and free of edge-case drift.

/// One renderable piece of a message line.
#[derive(Clone, Debug, PartialEq)]
pub enum Span {
    /// Plain or styled text.
    Text {
        text: String,
        bold: bool,
        italic: bool,
        code: bool,
        strike: bool,
    },
    /// A link: the label shows, the url is the destination.
    Link { label: String, url: String },
    /// `@user` mention.
    Mention(String),
    /// A custom emoji resolved from the cache: the image path.
    Emoji { path: String },
}

/// Parses one line of message text into spans. `emoji_path` resolves a
/// custom-emoji name to its cached image path; unknown names stay literal.
pub fn parse(text: &str, emoji_path: &dyn Fn(&str) -> Option<String>) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;

    'scan: while !rest.is_empty() {
        // Custom emoji: :name: with a cache hit wins over everything else.
        if let Some(after) = rest.strip_prefix(':')
            && let Some(end) = after.find(':')
        {
            let name = &after[..end];
            if is_emoji_name(name)
                && let Some(path) = emoji_path(name)
            {
                flush_plain(&mut plain, &mut spans);
                spans.push(Span::Emoji { path });
                rest = &after[end + 1..];
                continue 'scan;
            }
        }

        // Links: bare urls and [label](url).
        if let Some(url) = prefix_url(rest) {
            flush_plain(&mut plain, &mut spans);
            let length = url.len();
            spans.push(Span::Link {
                label: url.to_owned(),
                url: url.to_owned(),
            });
            rest = &rest[length..];
            continue 'scan;
        }
        if let Some(rest_after) = rest.strip_prefix('[')
            && let Some((label, url, tail)) = labeled_link(rest_after)
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(Span::Link { label, url });
            rest = tail;
            continue 'scan;
        }

        // Mentions: @name at a token boundary.
        if let Some(after) = rest.strip_prefix('@')
            && let Some(name) = prefix_token(after)
        {
            flush_plain(&mut plain, &mut spans);
            spans.push(Span::Mention(format!("@{name}")));
            rest = &after[name.len()..];
            continue 'scan;
        }

        // Emphasis: *bold*, _italic_, `code`, ~strike~ (single level).
        for (marker, bold, italic, code, strike) in [
            ('*', true, false, false, false),
            ('_', false, true, false, false),
            ('`', false, false, true, false),
            ('~', false, false, false, true),
        ] {
            if let Some(after) = rest.strip_prefix(marker)
                && let Some(end) = after.find(marker)
                && end > 0
            {
                flush_plain(&mut plain, &mut spans);
                spans.push(Span::Text {
                    text: after[..end].to_owned(),
                    bold,
                    italic,
                    code,
                    strike,
                });
                rest = &after[end + 1..];
                continue 'scan;
            }
        }

        // Nothing matched: one character into the plain buffer.
        let boundary = rest
            .char_indices()
            .nth(1)
            .map(|(index, _)| index)
            .unwrap_or(rest.len());
        plain.push_str(&rest[..boundary]);
        rest = &rest[boundary..];
    }
    flush_plain(&mut plain, &mut spans);
    spans
}

fn flush_plain(plain: &mut String, spans: &mut Vec<Span>) {
    if !plain.is_empty() {
        spans.push(Span::Text {
            text: std::mem::take(plain),
            bold: false,
            italic: false,
            code: false,
            strike: false,
        });
    }
}

/// Emoji names: letters, digits, `_`, `-`, `+` — custom and unicode sets.
fn is_emoji_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '+')
        })
}

/// A bare url at the start of `rest`, if any.
fn prefix_url(rest: &str) -> Option<&str> {
    let scheme_length = if rest.starts_with("https://") {
        8
    } else if rest.starts_with("http://") {
        7
    } else {
        return None;
    };
    let end = rest[scheme_length..]
        .find(|character: char| character.is_whitespace() || matches!(character, ')' | '<'))
        .map(|index| scheme_length + index)
        .unwrap_or(rest.len());
    if end > scheme_length {
        Some(&rest[..end])
    } else {
        None
    }
}

/// `[label](url)` after the opening bracket: returns label, url, tail.
fn labeled_link(rest: &str) -> Option<(String, String, &str)> {
    let label_end = rest.find(']')?;
    let label = &rest[..label_end];
    let after_label = &rest[label_end + 1..];
    let full_url = after_label.strip_prefix('(')?;
    let url_end = full_url.find(')')?;
    let url = &full_url[..url_end];
    if label.is_empty() || url.is_empty() {
        return None;
    }
    Some((label.to_owned(), url.to_owned(), &full_url[url_end + 1..]))
}

/// An identifier at the start of `rest` (mentions, token names).
fn prefix_token(rest: &str) -> Option<&str> {
    let end = rest
        .find(|character: char| {
            !(character.is_alphanumeric() || matches!(character, '_' | '.' | '-'))
        })
        .unwrap_or(rest.len());
    if end > 0 { Some(&rest[..end]) } else { None }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // tests assert the expected path
mod tests {
    use super::*;

    fn no_emoji(_: &str) -> Option<String> {
        None
    }

    fn cached(name: &str) -> Option<String> {
        (name == "heart_21").then(|| "/emoji/heart_21.gif".to_owned())
    }

    #[test]
    fn plain_text_stays_one_span() {
        let spans = parse("просто сообщение", &no_emoji);
        assert_eq!(spans.len(), 1);
        assert!(
            matches!(&spans[0], Span::Text { text, bold: false, .. } if text == "просто сообщение")
        );
    }

    #[test]
    fn emphasis_and_code_decode() {
        let spans = parse("*важно* и `код` и _наклон_", &no_emoji);
        assert_eq!(spans.len(), 5);
        assert!(matches!(&spans[0], Span::Text { bold: true, text, .. } if text == "важно"));
        assert!(matches!(&spans[2], Span::Text { code: true, text, .. } if text == "код"));
        assert!(matches!(&spans[4], Span::Text { italic: true, text, .. } if text == "наклон"));
    }

    #[test]
    fn links_and_mentions_decode() {
        let spans = parse("см. https://example.com/x и @rorikstr", &no_emoji);
        assert!(matches!(&spans[1], Span::Link { url, .. } if url == "https://example.com/x"));
        assert!(matches!(&spans[3], Span::Mention(name) if name == "@rorikstr"));
    }

    #[test]
    fn labeled_links_decode() {
        let spans = parse("[док](https://docs.rs)", &no_emoji);
        assert!(matches!(
            &spans[0],
            Span::Link { label, url } if label == "док" && url == "https://docs.rs"
        ));
    }

    #[test]
    fn cached_emoji_becomes_an_image_span() {
        let spans = parse("держи :heart_21: от меня", &cached);
        assert_eq!(spans.len(), 3);
        assert!(matches!(
            &spans[1],
            Span::Emoji { path } if path == "/emoji/heart_21.gif"
        ));
    }

    #[test]
    fn unknown_emoji_names_stay_literal() {
        let spans = parse(":не-эмодзи:", &cached);
        assert_eq!(spans.len(), 1);
        assert!(matches!(&spans[0], Span::Text { text, .. } if text == ":не-эмодзи:"));
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        let spans = parse("2 * 3 = 6 и _незакрыт", &no_emoji);
        // "2 " plain; "* 3 = 6 и _незакрыт" — no closing '*' → literal.
        assert!(spans.iter().all(|span| matches!(span, Span::Text { .. })));
    }
}
