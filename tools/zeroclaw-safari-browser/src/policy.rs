use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};
use url::{Host, Url};

const DENIED_DOMAINS: &[&str] = &["linkedin.com", "lnkd.in"];
const DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const LINK_DNS_BUDGET: Duration = Duration::from_secs(4);
const MAX_CONCURRENT_LOOKUPS: usize = 8;

fn validate_host(raw: &str) -> Result<String> {
    let host = raw.trim_end_matches('.').to_ascii_lowercase();
    if DENIED_DOMAINS
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        bail!("LinkedIn is blocked by the owner's hard no-messaging policy");
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
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        || host
            .bytes()
            .any(|byte| !byte.is_ascii_alphanumeric() && byte != b'-' && byte != b'.')
    {
        bail!("Only public DNS hostnames are allowed");
    }
    Ok(host)
}

fn parse_public_url(raw: &str) -> Result<(Url, String)> {
    let url = Url::parse(raw)?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Use a public HTTPS URL on port 443 without embedded credentials");
    }
    let Some(Host::Domain(host)) = url.host() else {
        bail!("Public DNS hostname required");
    };
    let host = validate_host(host)?;
    Ok((url, host))
}

async fn resolve_host(host: String) -> Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host.as_str(), 443))
        .await?
        .collect())
}

fn validate_addresses(addresses: &[SocketAddr]) -> Result<()> {
    if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
        bail!("DNS resolution includes a private, reserved, or unsupported address");
    }
    Ok(())
}

pub async fn validate_url(raw: &str) -> Result<Url> {
    let (url, host) = parse_public_url(raw)?;
    let addresses = timeout(DNS_LOOKUP_TIMEOUT, resolve_host(host))
        .await
        .context("Public hostname DNS lookup timed out")??;
    validate_addresses(&addresses)?;
    Ok(url)
}

/// Mask unsafe links, resolving each hostname once for this page read only.
pub async fn filter_link_destinations(state: &mut Value) -> Result<()> {
    filter_link_destinations_with(state, resolve_host, DNS_LOOKUP_TIMEOUT, LINK_DNS_BUDGET).await
}

async fn filter_link_destinations_with<R, F>(
    state: &mut Value,
    mut resolve: R,
    lookup_timeout: Duration,
    total_budget: Duration,
) -> Result<()>
where
    R: FnMut(String) -> F,
    F: Future<Output = Result<Vec<SocketAddr>>> + Send + 'static,
{
    let Some(controls) = state.get_mut("controls").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let mut hosts: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (index, control) in controls.iter_mut().enumerate() {
        let Some(href) = control.get("href").and_then(Value::as_str) else {
            continue;
        };
        if href.is_empty() {
            continue;
        }
        // Every URL needs its own syntax and policy check, even when another
        // link on its hostname has already passed.
        if let Ok((_, host)) = parse_public_url(href) {
            hosts
                .entry(host)
                .or_default()
                .push((index, href.to_owned()));
        }
        // Start closed and restore only links whose lookup succeeds in time.
        // Queued and in-flight links therefore stay masked at budget expiry.
        control["href"] = Value::String("[blocked]".to_owned());
    }

    let deadline = Instant::now() + total_budget;
    let mut hosts = hosts.into_iter();
    let mut lookups = JoinSet::new();
    loop {
        if Instant::now() >= deadline {
            break;
        }
        while lookups.len() < MAX_CONCURRENT_LOOKUPS {
            let Some((host, indices)) = hosts.next() else {
                break;
            };
            let lookup = resolve(host);
            lookups.spawn(async move {
                let allowed = match timeout(lookup_timeout, lookup).await {
                    Ok(Ok(addresses)) => validate_addresses(&addresses).is_ok(),
                    _ => false,
                };
                (indices, allowed)
            });
        }
        let result = match timeout_at(deadline, lookups.join_next()).await {
            Ok(Some(result)) => result,
            Ok(None) | Err(_) => break,
        };
        if Instant::now() >= deadline {
            break;
        }
        let (indices, allowed) = result.context("Link destination validation failed")?;
        if allowed {
            for (index, href) in indices {
                controls[index]["href"] = Value::String(href);
            }
        }
    }
    Ok(())
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

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => public_v4(ip),
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            (segments[0] & 0xe000) == 0x2000
                && segments[0] != 0x2002
                && !(segments[0] == 0x2001 && (segments[1] < 0x200 || segments[1] == 0xdb8))
                && !(segments[0] == 0x3fff && (segments[1] & 0xf000) == 0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    fn public_addresses() -> Vec<SocketAddr> {
        vec!["93.184.215.14:443".parse().unwrap()]
    }

    #[test]
    fn allows_public_https_syntax_and_addresses() {
        assert!(parse_public_url("https://example.com/").is_ok());
        assert!(validate_addresses(&public_addresses()).is_ok());
    }

    #[tokio::test]
    async fn blocks_linkedin_local_and_non_https() {
        for url in [
            "https://linkedin.com/messaging/",
            "https://lnkd.in/x",
            "https://localhost/",
            "https://127.0.0.1/",
            "http://example.com/",
            "file:///etc/passwd",
        ] {
            assert!(validate_url(url).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn resolves_duplicate_hosts_once_and_checks_every_url() {
        let mut state = json!({"controls": [
            {"href": "https://example.com/a"},
            {"href": "https://EXAMPLE.COM/b"},
            {"href": "https://example.com./c"},
            {"href": "http://example.com/d"},
            {"href": "https://user:secret@example.com/e"},
            {"href": "https://example.com:8443/f"},
            {"href": ""},
            {"text": "button"}
        ]});
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed_calls = calls.clone();
        filter_link_destinations_with(
            &mut state,
            move |host| {
                observed_calls.lock().unwrap().push(host);
                async { Ok(public_addresses()) }
            },
            DNS_LOOKUP_TIMEOUT,
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(*calls.lock().unwrap(), ["example.com"]);
        for index in 0..3 {
            assert_ne!(state["controls"][index]["href"], "[blocked]");
        }
        for index in 3..6 {
            assert_eq!(state["controls"][index]["href"], "[blocked]");
        }
        assert_eq!(state["controls"][6]["href"], "");
        assert!(state["controls"][7].get("href").is_none());
    }

    #[tokio::test]
    async fn masks_policy_blocks_without_dns_and_all_unsafe_host_links() {
        let blocked_urls = [
            "https://linkedin.com/messaging/",
            "https://www.linkedin.com/",
            "https://lnkd.in/x",
            "https://localhost/",
            "https://service.internal/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "not a URL",
            "https://unsafe.example/a",
            "https://unsafe.example/b",
            "https://empty.example/",
            "https://error.example/",
        ];
        let mut state = json!({"controls": blocked_urls.map(|href| json!({"href": href}))});
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed_calls = calls.clone();
        filter_link_destinations_with(
            &mut state,
            move |host| {
                observed_calls.lock().unwrap().push(host.clone());
                async move {
                    match host.as_str() {
                        "unsafe.example" => Ok(vec![
                            "93.184.215.14:443".parse().unwrap(),
                            "10.0.0.1:443".parse().unwrap(),
                        ]),
                        "empty.example" => Ok(Vec::new()),
                        "error.example" => bail!("resolver unavailable"),
                        _ => panic!("Policy-blocked host must not reach DNS: {host}"),
                    }
                }
            },
            DNS_LOOKUP_TIMEOUT,
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(calls.lock().unwrap().len(), 3);
        for control in state["controls"].as_array().unwrap() {
            assert_eq!(control["href"], "[blocked]");
        }
    }

    #[tokio::test]
    async fn page_with_160_links_on_four_hosts_uses_four_lookups() {
        let controls: Vec<_> = (0..160)
            .map(|index| json!({"href": format!("https://host{}.example/page/{index}", index % 4)}))
            .collect();
        let mut state = json!({"controls": controls});
        let original = state.clone();
        let calls = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let observed_calls = calls.clone();
        filter_link_destinations_with(
            &mut state,
            move |host| {
                *observed_calls.lock().unwrap().entry(host).or_default() += 1;
                async { Ok(public_addresses()) }
            },
            DNS_LOOKUP_TIMEOUT,
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert!(calls.values().all(|count| *count == 1));
        assert_eq!(state, original);
    }

    #[tokio::test]
    async fn resolves_independent_hosts_concurrently_with_a_limit() {
        let controls: Vec<_> = (0..25)
            .map(|index| json!({"href": format!("https://host{index}.example/")}))
            .collect();
        let mut state = json!({"controls": controls});
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        filter_link_destinations_with(
            &mut state,
            {
                let active = active.clone();
                let peak = peak.clone();
                let calls = calls.clone();
                move |_| {
                    let active = active.clone();
                    let peak = peak.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(count, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        active.fetch_sub(1, Ordering::SeqCst);
                        Ok(public_addresses())
                    }
                }
            },
            DNS_LOOKUP_TIMEOUT,
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 25);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(peak.load(Ordering::SeqCst), MAX_CONCURRENT_LOOKUPS);
        for control in state["controls"].as_array().unwrap() {
            assert_ne!(control["href"], "[blocked]");
        }
    }

    #[tokio::test]
    async fn masks_timeout_and_does_not_cache_trust_between_reads() {
        let original = json!({"controls": [
            {"href": "https://example.com/a"},
            {"href": "https://example.com/b"}
        ]});
        let mut first_read = original.clone();
        filter_link_destinations_with(
            &mut first_read,
            |_| async { Ok(public_addresses()) },
            DNS_LOOKUP_TIMEOUT,
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        assert_eq!(first_read, original);

        let mut second_read = original;
        filter_link_destinations_with(
            &mut second_read,
            |_| std::future::pending::<Result<Vec<SocketAddr>>>(),
            Duration::from_millis(5),
            LINK_DNS_BUDGET,
        )
        .await
        .unwrap();
        for control in second_read["controls"].as_array().unwrap() {
            assert_eq!(control["href"], "[blocked]");
        }
    }

    #[tokio::test]
    async fn overall_budget_masks_in_flight_and_queued_hosts() {
        let controls: Vec<_> = (0..160)
            .map(|index| json!({"href": format!("https://slow{index}.example/")}))
            .collect();
        let mut state = json!({"controls": controls});
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_calls = calls.clone();
        let budget = Duration::from_millis(10);
        let started = Instant::now();
        timeout(
            Duration::from_secs(1),
            filter_link_destinations_with(
                &mut state,
                move |_| {
                    observed_calls.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<Result<Vec<SocketAddr>>>()
                },
                DNS_LOOKUP_TIMEOUT,
                budget,
            ),
        )
        .await
        .expect("The page-wide budget must finish before any per-host timeout")
        .unwrap();
        assert!(started.elapsed() >= budget);
        assert_eq!(calls.load(Ordering::SeqCst), MAX_CONCURRENT_LOOKUPS);
        for control in state["controls"].as_array().unwrap() {
            assert_eq!(control["href"], "[blocked]");
        }
    }
}
