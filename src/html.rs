//! A small, tolerant scanner for the HTML people write in markdown: tags,
//! their attributes and comments. It does not build a tree.

/// One piece of an HTML string.
#[derive(Debug, PartialEq)]
pub enum Piece<'a> {
    Text(&'a str),
    /// A `<!-- comment -->`, or one left open at the end, as written.
    Comment(&'a str),
    Tag(Tag<'a>),
}

/// An opening or closing tag.
#[derive(Debug, PartialEq)]
pub struct Tag<'a> {
    /// The tag's name in lower case.
    pub name: String,
    pub close: bool,
    /// The tag as written.
    pub raw: &'a str,
    /// What follows the name, up to the `>`.
    attrs: &'a str,
}

impl Tag<'_> {
    /// The value of the attribute `name`, in any case. An attribute with no
    /// value has an empty one.
    pub fn attr(&self, name: &str) -> Option<&str> {
        let mut rest = self.attrs;
        loop {
            rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
            if rest.is_empty() {
                return None;
            }
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
                .unwrap_or(rest.len());
            let (key, after) = rest.split_at(end);
            let after = after.trim_start();
            let (value, after) = match after.strip_prefix('=') {
                Some(v) => value(v.trim_start()),
                None => ("", after),
            };
            if key.eq_ignore_ascii_case(name) {
                return Some(value);
            }
            rest = after;
        }
    }
}

/// An attribute's value, quoted or not, and what follows it.
fn value(s: &str) -> (&str, &str) {
    if let Some(q) = s.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let inner = &s[1..];
        return match inner.find(q) {
            Some(end) => (&inner[..end], &inner[end + 1..]),
            None => (inner, ""),
        };
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    s.split_at(end)
}

/// `html` split into text, comments and tags. A `<` that starts neither is
/// text.
pub fn pieces(html: &str) -> Vec<Piece<'_>> {
    let mut out = Vec::new();
    let mut text = 0;
    let mut at = 0;
    while let Some(i) = html[at..].find('<').map(|i| at + i) {
        let found = match html[i..].strip_prefix("<!--") {
            Some(body) => {
                let end = body.find("-->").map_or(html.len(), |e| i + 4 + e + 3);
                Some((Piece::Comment(&html[i..end]), end))
            }
            None => tag(&html[i..]).map(|t| {
                let end = i + t.raw.len();
                (Piece::Tag(t), end)
            }),
        };
        match found {
            Some((piece, end)) => {
                if text < i {
                    out.push(Piece::Text(&html[text..i]));
                }
                out.push(piece);
                text = end;
                at = end;
            }
            None => at = i + 1,
        }
    }
    if text < html.len() {
        out.push(Piece::Text(&html[text..]));
    }
    out
}

/// The tag at the start of `s`, if one is there.
fn tag(s: &str) -> Option<Tag<'_>> {
    let body = s.strip_prefix('<')?;
    let (close, body) = match body.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, body),
    };
    let name_len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(body.len());
    if name_len == 0 || !body.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let (name, rest) = body.split_at(name_len);
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
        return None;
    }
    // Find the `>`, skipping any inside quoted values.
    let mut quote = None;
    for (i, c) in rest.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '>') => {
                let len = s.len() - rest.len() + i + 1;
                return Some(Tag {
                    name: name.to_ascii_lowercase(),
                    close,
                    raw: &s[..len],
                    attrs: &rest[..i],
                });
            }
            (None, '<') => return None,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(html: &str) -> Vec<String> {
        pieces(html)
            .iter()
            .map(|p| match p {
                Piece::Text(t) => format!("text {t:?}"),
                Piece::Comment(_) => "comment".to_string(),
                Piece::Tag(t) => format!("{}{}", if t.close { "/" } else { "" }, t.name),
            })
            .collect()
    }

    #[test]
    fn splits_text_tags_and_comments() {
        assert_eq!(
            tags("a <B>b</b><br/> <!-- c\n d --> 1 < 2 <3 e"),
            [
                "text \"a \"",
                "b",
                "text \"b\"",
                "/b",
                "br",
                "text \" \"",
                "comment",
                "text \" 1 < 2 <3 e\"",
            ]
        );
        assert_eq!(tags("x <!-- open"), ["text \"x \"", "comment"]);
        assert_eq!(tags("<a href=\"x>y\">"), ["a"]);
        assert_eq!(tags("<a <b>"), ["text \"<a \"", "b"]);
    }

    #[test]
    fn reads_attributes_quoted_or_not() {
        let html = "<img SRC=\"a b.png\" alt='it''s' width=200 hidden/>";
        let pieces = pieces(html);
        let [Piece::Tag(img)] = pieces.as_slice() else {
            panic!("{pieces:?}");
        };
        assert_eq!(img.raw, html);
        assert_eq!(img.attr("src"), Some("a b.png"));
        assert_eq!(img.attr("alt"), Some("it"));
        assert_eq!(img.attr("width"), Some("200"));
        assert_eq!(img.attr("hidden"), Some(""));
        assert_eq!(img.attr("title"), None);
    }
}
