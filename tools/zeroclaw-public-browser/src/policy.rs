use anyhow::{Result, bail};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use url::{Host, Url};

// Single source for the non-overridable browser exclusion. Applies to initial
// navigation, redirects, frames, scripts, WebSockets and all tunneled requests.
const DENIED_DOMAINS: &[&str] = &["linkedin.com", "lnkd.in"];

pub fn validate_host(raw: &str) -> Result<String> {
    let host = raw.trim_end_matches('.').to_ascii_lowercase();
    if DENIED_DOMAINS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
    {
        bail!(
            "LinkedIn is blocked by the owner's hard no-messaging policy. Approval cannot override this."
        );
    }
    if host.is_empty()
        || !host.contains('.')
        || host.parse::<IpAddr>().is_ok()
        || [
            "localhost",
            "local",
            "internal",
            "home",
            "lan",
            "test",
            "invalid",
        ]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
        || host
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && b != b'-' && b != b'.')
    {
        bail!(
            "Only public DNS hostnames are allowed; local/private hosts and literal IP addresses are blocked."
        );
    }
    Ok(host)
}

pub fn validate_url(raw: &str) -> Result<Url> {
    let url = Url::parse(raw)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Use a public HTTPS URL on port 443, without embedded credentials.");
    }
    let Some(Host::Domain(host)) = url.host() else {
        bail!("Public DNS hostname required.");
    };
    validate_host(host)?;
    Ok(url)
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(matches!(a, 0 | 10 | 127 | 224..=255)
        || a == 100 && (64..=127).contains(&b)
        || a == 169 && b == 254
        || a == 172 && (16..=31).contains(&b)
        || a == 192 && (b == 168 || b == 0 || (b == 88 && c == 99))
        || a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100))
        || a == 203 && b == 0 && c == 113)
}

pub fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            // Fail closed outside global unicast and on transition/documentation ranges.
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && s[0] != 0x2002
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}

pub fn select_public_address(addresses: &[SocketAddr]) -> Result<SocketAddr> {
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|a| !public_ip(a.ip()) || a.port() != 443)
    {
        bail!("DNS resolution includes a private, reserved, or unsupported address.");
    }
    // Prefer IPv4 on hosts without reliable IPv6 routing. The selected address
    // is used directly by connect, so no second DNS lookup/rebinding can occur.
    addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addresses.first())
        .copied()
        .ok_or_else(|| anyhow::Error::msg("No public DNS address."))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_sites_and_queries_are_allowed() {
        for u in [
            "https://example.com",
            "https://services.petsmart.com/grooming/reschedule/entry",
            "https://www.google.com/search?q=dog+grooming",
        ] {
            assert!(validate_url(u).is_ok(), "{u}");
        }
    }
    #[test]
    fn linkedin_and_bypass_forms_are_denied() {
        for u in [
            "https://linkedin.com",
            "https://www.linkedin.com/messaging/",
            "https://api.linkedin.com/v2/messages",
            "https://www.LINKEDIN.com./",
            "https://lnkd.in/a",
            "https://nested.api.linkedin.com",
            "https://example.com@linkedin.com",
            "https://linkedin.com@other.example.com",
        ] {
            assert!(validate_url(u).is_err(), "{u}");
        }
    }
    #[test]
    fn local_and_non_web_urls_are_denied() {
        for u in [
            "http://example.com",
            "https://localhost",
            "https://foo.local",
            "https://foo.internal",
            "https://127.0.0.1",
            "https://2130706433",
            "https://[::1]",
            "https://example.com:8443",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/html,test",
            "chrome://settings",
        ] {
            assert!(validate_url(u).is_err(), "{u}");
        }
    }
    #[test]
    fn mixed_dns_and_reserved_addresses_fail_closed() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "100.64.1.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "::1",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:808:808::1",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(
            select_public_address(&[
                "8.8.8.8:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap()
            ])
            .is_err()
        );
    }
}
