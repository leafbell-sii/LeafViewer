//! GFM 확장 자동 링크: 글 속의 `https://…`, `http://…`, `www.…`를 링크로 (FR-M01).
//! pulldown-cmark는 `<url>` 형식만 링크로 만든다.

use leaf_doc::flow::Inline;

/// 링크·코드 안은 건드리지 않고 글자 조각만 바꾼다.
pub fn linkify(v: &mut Vec<Inline>) {
    let mut i = 0;
    while i < v.len() {
        match &mut v[i] {
            Inline::Text(t) => {
                let parts = split(t);
                if parts.len() > 1 || matches!(parts.first(), Some(Inline::Link { .. })) {
                    let n = parts.len();
                    v.splice(i..=i, parts);
                    i += n;
                    continue;
                }
            }
            Inline::Strong(c) | Inline::Emph(c) | Inline::Strike(c) => linkify(c),
            _ => {}
        }
        i += 1;
    }
}

fn split(t: &str) -> Vec<Inline> {
    let mut out = Vec::new();
    let mut rest = t;
    let mut plain_start = 0usize;
    let mut consumed = 0usize;
    while let Some((s, e)) = find_url(rest) {
        let abs_s = consumed + s;
        let abs_e = consumed + e;
        if abs_s > plain_start {
            out.push(Inline::Text(t[plain_start..abs_s].to_string()));
        }
        let url = &t[abs_s..abs_e];
        let href = if url.to_ascii_lowercase().starts_with("www.") {
            format!("http://{url}")
        } else {
            url.to_string()
        };
        out.push(Inline::Link {
            href,
            children: vec![Inline::Text(url.to_string())],
        });
        plain_start = abs_e;
        consumed = abs_e;
        rest = &t[abs_e..];
    }
    if plain_start < t.len() || out.is_empty() {
        out.push(Inline::Text(t[plain_start..].to_string()));
    }
    out
}

/// 첫 URL의 바이트 범위
fn find_url(s: &str) -> Option<(usize, usize)> {
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    loop {
        let cand = ["https://", "http://", "www."]
            .iter()
            .filter_map(|p| lower[from..].find(p).map(|i| (from + i, p.len())))
            .min_by_key(|(i, _)| *i)?;
        let (start, plen) = cand;
        // 앞 글자가 글자·숫자면 단어 중간 (예: "awww.x")
        let ok_prefix = s[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '/' && c != '.');
        let end = url_end(s, start);
        if end <= start + plen {
            from = start + plen;
            continue;
        }
        // 접두사 뒤에 최소 한 글자 + 점(도메인) 필요
        let body = &s[start + plen..end];
        let has_domain = body.split('/').next().is_some_and(|d| {
            d.contains('.') || (!lower[start..].starts_with("www.") && !d.is_empty())
        });
        if ok_prefix && end > start + plen && has_domain {
            return Some((start, end));
        }
        from = start + plen;
    }
}

fn url_end(s: &str, start: usize) -> usize {
    let mut end = s.len();
    for (i, c) in s[start..].char_indices() {
        if c.is_whitespace() || c == '<' || c == '>' || c == '"' || (c as u32) < 0x20 {
            end = start + i;
            break;
        }
        // 한글 등이 바로 붙으면 거기서 끊는다 (예: "https://x.y에서")
        if !c.is_ascii() {
            end = start + i;
            break;
        }
    }
    // 끝의 구두점은 링크에서 뺀다. 괄호는 짝이 맞을 때만 남긴다.
    loop {
        let u = &s[start..end];
        let Some(last) = u.chars().next_back() else {
            break;
        };
        let drop = match last {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' | '_' | '~' => true,
            ')' => u.matches('(').count() < u.matches(')').count(),
            _ => false,
        };
        if !drop {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links(s: &str) -> Vec<String> {
        split(s)
            .into_iter()
            .filter_map(|i| match i {
                Inline::Link { href, .. } => Some(href),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn finds_urls() {
        assert_eq!(links("see https://a.b/c."), vec!["https://a.b/c"]);
        assert_eq!(links("(www.x.com)"), vec!["http://www.x.com"]);
        assert_eq!(links("https://w.org/a_(b)!"), vec!["https://w.org/a_(b)"]);
        assert_eq!(links("주소는 https://x.y에서"), vec!["https://x.y"]);
        assert_eq!(
            links("two http://a.b and http://c.d"),
            vec!["http://a.b", "http://c.d"]
        );
        assert!(links("awww.x.com http:// www.").is_empty());
        assert!(links("plain text").is_empty());
    }

    #[test]
    fn keeps_surrounding_text() {
        let v = split("a https://x.y b");
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], Inline::Text("a ".into()));
        assert_eq!(v[2], Inline::Text(" b".into()));
    }
}
