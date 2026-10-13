//! Harness for the end-to-end acceptance test (`tests/e2e.rs`): anvil with
//! the zkVM contracts and the origin-3 census contract, sequencer nodes as
//! subprocesses, the seeded voter fixture and the polling helpers. `demo`
//! holds the demo elections of `tests/demo.rs`.
#![forbid(unsafe_code)]

pub mod census;
pub mod chain;
pub mod cost;
pub mod demo;
pub mod dkg;
pub mod fixture;
pub mod l1;
pub mod negative;
pub mod net;
pub mod node;
pub mod proxy;
pub mod wait;

/// Replaces every `http(s)://...` URL in `s` with `scheme://host`: reqwest
/// errors embed the request URL, whose path or userinfo may hold an RPC key.
pub fn redact_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let scheme_start = rest[..i]
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_ascii_alphabetic())
            .map_or(0, |(p, c)| p + c.len_utf8());
        let scheme = &rest[scheme_start..i];
        if scheme != "http" && scheme != "https" {
            out.push_str(&rest[..i + 3]);
            rest = &rest[i + 3..];
            continue;
        }
        out.push_str(&rest[..i + 3]);
        let after = &rest[i + 3..];
        let end = after
            .find(|c: char| c.is_whitespace() || "()<>\"',".contains(c))
            .unwrap_or(after.len());
        let auth = &after[..end];
        let auth = auth.split(['/', '?', '#']).next().unwrap_or("");
        let host = auth.rsplit('@').next().unwrap_or("");
        out.push_str(host);
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::redact_urls;

    #[test]
    fn urls_are_reduced_to_scheme_and_host() {
        assert_eq!(
            redact_urls(
                "error sending request for url (https://user:pw@host.example/v2/KEY?x=1): boom"
            ),
            "error sending request for url (https://host.example): boom"
        );
        assert_eq!(redact_urls("a http://h:8545/k b"), "a http://h:8545 b");
        assert_eq!(redact_urls("no url, ftp://x/y"), "no url, ftp://x/y");
        let r = redact_urls("x \u{201c}https://u:p@h/k\u{201d} y");
        assert!(!r.contains("u:p") && !r.contains("/k"), "{r}");
    }
}
