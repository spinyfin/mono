//! Host identity carried in the client certificate.
//!
//! A spoke's client certificate names exactly one host through a URI subject
//! alternative name, `urn:boss:host:<opaque-host-id>`. `Hello.host_id` must
//! equal that name: the certificate authenticates the host and `Hello` only
//! claims it. This module owns the spelling so the hub (which mints and checks
//! certificates) and the spoke (which asserts the id) cannot disagree.

/// Prefix of the host URI SAN.
pub const HOST_URN_PREFIX: &str = "urn:boss:host:";

/// Longest accepted host id, in bytes.
pub const MAX_HOST_ID_BYTES: usize = 128;

/// Whether `id` is usable as an opaque host id: non-empty, bounded, and drawn
/// from `[A-Za-z0-9._-]` so it embeds in a URN without escaping and cannot
/// smuggle a second identity (no `:`, `/`, whitespace or control characters).
pub fn is_valid_host_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_HOST_ID_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The URI SAN for `host_id`, or `None` when the id is not valid.
pub fn host_urn(host_id: &str) -> Option<String> {
    is_valid_host_id(host_id).then(|| format!("{HOST_URN_PREFIX}{host_id}"))
}

/// The host id named by a SAN URI, or `None` when `uri` is not a well-formed
/// Boss host URN.
pub fn host_id_from_urn(uri: &str) -> Option<&str> {
    uri.strip_prefix(HOST_URN_PREFIX).filter(|id| is_valid_host_id(id))
}

/// Whether a certificate presenting `san_uris` is authorised to speak for
/// `claimed_host_id`.
///
/// Every Boss host URN among the SANs must name the claimed host, and at least
/// one must be present. A certificate that names several different hosts
/// authenticates none of them, so ambiguity fails closed.
pub fn certificate_names_host<'a>(claimed_host_id: &str, san_uris: impl IntoIterator<Item = &'a str>) -> bool {
    let mut named = san_uris.into_iter().filter_map(host_id_from_urn);
    let Some(first) = named.next() else {
        return false;
    };
    first == claimed_host_id && named.all(|id| id == first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urn_round_trips_for_valid_ids() {
        for id in ["local", "mac-mini.1", "Host_9", "a"] {
            let urn = host_urn(id).expect("valid id");
            assert_eq!(urn, format!("urn:boss:host:{id}"));
            assert_eq!(host_id_from_urn(&urn), Some(id));
        }
    }

    #[test]
    fn rejects_ids_that_could_smuggle_a_second_identity() {
        for id in ["", "a:b", "a/b", "a b", "a\nb", "é", &"x".repeat(MAX_HOST_ID_BYTES + 1)] {
            assert!(!is_valid_host_id(id), "{id:?} must be rejected");
            assert_eq!(host_urn(id), None);
        }
        assert_eq!(host_id_from_urn("urn:boss:host:"), None);
        assert_eq!(host_id_from_urn("urn:boss:host:a:b"), None);
        assert_eq!(host_id_from_urn("urn:other:host:a"), None);
    }

    #[test]
    fn certificate_must_name_exactly_the_claimed_host() {
        assert!(certificate_names_host("h1", ["urn:boss:host:h1"]));
        assert!(certificate_names_host(
            "h1",
            ["https://example.test", "urn:boss:host:h1"]
        ));
        assert!(!certificate_names_host("h2", ["urn:boss:host:h1"]));
        assert!(!certificate_names_host("h1", ["https://example.test"]));
        assert!(!certificate_names_host("h1", []));
    }

    #[test]
    fn certificate_naming_two_hosts_authenticates_neither() {
        let sans = ["urn:boss:host:h1", "urn:boss:host:h2"];
        assert!(!certificate_names_host("h1", sans));
        assert!(!certificate_names_host("h2", sans));
    }
}
