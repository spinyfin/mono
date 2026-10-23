//! Reading the fields the hub keys on out of a presented certificate.

use boss_protocol_spoke::identity::host_id_from_urn;
use x509_parser::extensions::GeneralName;

/// What the lifecycle needs from a parsed host certificate.
pub(crate) struct LeafInfo {
    /// Hex of the DER serial; the key the store knows the certificate by.
    pub serial: String,
    /// `notAfter`, in Unix seconds.
    pub not_after: i64,
    /// The URI subject alternative names.
    pub san_uris: Vec<String>,
}

pub(crate) fn parse_leaf(der: &[u8]) -> Option<LeafInfo> {
    let (rest, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    if !rest.is_empty() {
        return None;
    }
    let san_uris = match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .filter_map(|name| match name {
                GeneralName::URI(uri) => Some((*uri).to_owned()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some(LeafInfo {
        serial: hex(cert.raw_serial()),
        not_after: cert.validity().not_after.timestamp(),
        san_uris,
    })
}

/// The single host a certificate names, or `None` when it names none or
/// several. A certificate naming two hosts authenticates neither.
pub(crate) fn named_host(san_uris: &[String]) -> Option<&str> {
    let mut named = san_uris.iter().filter_map(|uri| host_id_from_urn(uri));
    let first = named.next()?;
    named.all(|id| id == first).then_some(first)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn a_certificate_must_name_exactly_one_host() {
        assert_eq!(named_host(&uris(&["urn:boss:host:a"])), Some("a"));
        assert_eq!(
            named_host(&uris(&["https://x.test", "urn:boss:host:a", "urn:boss:host:a"])),
            Some("a")
        );
        assert_eq!(named_host(&uris(&["urn:boss:host:a", "urn:boss:host:b"])), None);
        assert_eq!(named_host(&uris(&["https://x.test"])), None);
        assert_eq!(named_host(&[]), None);
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        assert_eq!(hex(&[0x00, 0x0a, 0xff]), "000aff");
    }

    #[test]
    fn garbage_and_trailing_bytes_are_not_certificates() {
        assert!(parse_leaf(b"not a certificate").is_none());
    }
}
