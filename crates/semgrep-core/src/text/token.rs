//! Code-aware tokenizer for BM25.
//!
//! Splits on non-alphanumerics, then splits identifiers on `_` and
//! camelCase/PascalCase boundaries. Emits both the whole (lowercased)
//! identifier and its subtokens, so `getUserName` matches "get user name"
//! and `getusername` alike.

/// Tokenize `text`, invoking `emit` for every token (lowercased).
pub fn for_each_token(text: &str, mut emit: impl FnMut(&str)) {
    let mut buf = String::with_capacity(32);
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if raw.is_empty() {
            continue;
        }
        for word in raw.split('_') {
            if word.is_empty() {
                continue;
            }
            let sub_count = split_camel(word, |sub| {
                if sub.chars().count() >= 2 {
                    buf.clear();
                    buf.extend(sub.chars().flat_map(|c| c.to_lowercase()));
                    emit(&buf);
                }
            });
            // Whole identifier too, when it actually decomposed.
            if sub_count > 1 && word.chars().count() >= 2 {
                buf.clear();
                buf.extend(word.chars().flat_map(|c| c.to_lowercase()));
                emit(&buf);
            }
        }
    }
}

pub fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for_each_token(text, |t| out.push(t.to_string()));
    out
}

/// Split camelCase/PascalCase/ALLCAPSTail boundaries; returns subtoken count.
fn split_camel(word: &str, mut emit: impl FnMut(&str)) -> usize {
    let chars: Vec<char> = word.chars().collect();
    let byte_idx: Vec<usize> = word.char_indices().map(|(i, _)| i).collect();
    let mut count = 0;
    let mut start = 0;
    for i in 1..chars.len() {
        let prev = chars[i - 1];
        let cur = chars[i];
        let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
        let boundary =
            // aB — lower/digit followed by upper
            ((prev.is_lowercase() || prev.is_ascii_digit()) && cur.is_uppercase())
            // ABb — end of an acronym run: upper followed by upper+lower
            || (prev.is_uppercase() && cur.is_uppercase() && next_lower);
        if boundary {
            emit(&word[byte_idx[start]..byte_idx[i]]);
            count += 1;
            start = i;
        }
    }
    emit(&word[byte_idx.get(start).copied().unwrap_or(0)..]);
    count + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_snake_and_camel() {
        assert_eq!(tokens("get_user_name"), ["get", "user", "name"]);
        assert_eq!(tokens("getUserName"), ["get", "user", "name", "getusername"]);
        assert_eq!(tokens("HTTPServerError"), ["http", "server", "error", "httpservererror"]);
    }

    #[test]
    fn lowercases_and_drops_short() {
        assert_eq!(tokens("A x FooBar"), ["foo", "bar", "foobar"]);
        assert_eq!(tokens("if (x) { y=2; }"), ["if"] as [&str; 1]);
    }

    #[test]
    fn prose_is_plain_words() {
        assert_eq!(tokens("The quick brown fox."), ["the", "quick", "brown", "fox"]);
    }

    #[test]
    fn digits_stick_to_identifiers() {
        assert_eq!(tokens("sha256sum utf8"), ["sha256sum", "utf8"]);
    }
}
