//! Just enough XML for S3 and Azure listing and upload responses: the
//! text of named elements, in order. No attributes, namespaces or CDATA,
//! which those responses do not use for the fields read here.

/// Every `<tag>...</tag>` body in `xml`, in order (bodies may hold other
/// elements).
pub fn elements<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let body = &rest[start + open.len()..];
        match body.find(&close) {
            Some(end) => {
                out.push(&body[..end]);
                rest = &body[end + close.len()..];
            }
            None => break,
        }
    }
    out
}

/// The unescaped text of the first `<tag>` in `xml`.
pub fn text(xml: &str, tag: &str) -> Option<String> {
    elements(xml, tag).first().map(|t| unescape(t))
}

pub fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elements_and_text() {
        let xml = "<R><Contents><Key>a&amp;b</Key><Size>3</Size></Contents><Contents><Key>c</Key></Contents><IsTruncated>false</IsTruncated></R>";
        let contents = elements(xml, "Contents");
        assert_eq!(contents.len(), 2);
        assert_eq!(text(contents[0], "Key").as_deref(), Some("a&b"));
        assert_eq!(text(xml, "IsTruncated").as_deref(), Some("false"));
        assert_eq!(text(xml, "Missing"), None);
    }
}
