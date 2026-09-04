//! Stateless protocol boundaries for the call-recording companion.
//!
//! The caller owns consent, call authorization, nonce lifetime, the canonical
//! public URL, and the trusted OpenClaw upstream. No helper here establishes
//! those facts or performs I/O.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::hmac;
use std::collections::{BTreeMap, BTreeSet};

pub type SafeResult<T> = Result<T, &'static str>;
pub type Form = BTreeMap<String, Vec<String>>;

const MAX_FORM_BYTES: usize = 32 * 1024;
const XML_DECLARATION: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>";

/// Decode form data without lossy UTF-8 replacement or discarded duplicates.
pub fn parse_form(body: &[u8]) -> SafeResult<Form> {
    if body.len() > MAX_FORM_BYTES {
        return Err("form_too_large");
    }
    let body = std::str::from_utf8(body).map_err(|_| "form_invalid_utf8")?;
    let mut form = Form::new();
    for pair in body.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        form.entry(decode_component(name)?)
            .or_default()
            .push(decode_component(value)?);
    }
    Ok(form)
}

fn decode_component(component: &str) -> SafeResult<String> {
    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        match bytes[offset] {
            b'+' => decoded.push(b' '),
            b'%' => {
                let high = bytes.get(offset + 1).and_then(|byte| hex(*byte));
                let low = bytes.get(offset + 2).and_then(|byte| hex(*byte));
                match (high, low) {
                    (Some(high), Some(low)) => decoded.push((high << 4) | low),
                    _ => return Err("form_invalid_percent_escape"),
                }
                offset += 2;
            }
            byte => decoded.push(byte),
        }
        offset += 1;
    }
    String::from_utf8(decoded).map_err(|_| "form_invalid_utf8")
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Ambiguous security-relevant fields are never reduced to a first/last value.
pub fn one<'a>(form: &'a Form, key: &str) -> Option<&'a str> {
    match form.get(key)?.as_slice() {
        [value] => Some(value.as_str()),
        _ => None,
    }
}

/// Verify the URL and every decoded form field using Twilio's HMAC-SHA1 format.
///
/// The public URL must be the exact external URL Twilio requested, including
/// its query string; do not substitute a reverse proxy's internal URL.
pub fn verify_signature(public_url: &str, form: &Form, signature: &str, token: &str) -> bool {
    if token.is_empty() || public_url.is_empty() || signature.len() != 28 {
        return false;
    }
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    if signature.len() != 20 {
        return false;
    }
    let mut payload = public_url.as_bytes().to_vec();
    for (name, values) in form {
        let values: BTreeSet<&str> = values.iter().map(String::as_str).collect();
        for value in values {
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(value.as_bytes());
        }
    }
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, token.as_bytes());
    hmac::verify(&key, &payload, &signature).is_ok()
}

pub fn valid_sid(value: &str, prefix: &str) -> bool {
    prefix.len() == 2
        && prefix.bytes().all(|byte| byte.is_ascii_alphabetic())
        && value.len() == 34
        && value.starts_with(prefix)
        && value.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
}

/// The action handler must treat 2, empty, or any other input as unrecorded.
/// URLs supplied to XML generators must come from the caller's validated config.
pub fn consent_xml(callback_url: &str) -> String {
    format!(
        "{XML_DECLARATION}\n<Response>\n  <Gather input=\"dtmf\" numDigits=\"1\" timeout=\"5\" actionOnEmptyResult=\"true\" method=\"POST\" action=\"{}\">\n    <Say>This call is handled by an AI assistant. With your permission, the conversation will be recorded and sent to the call owner in a private Telegram chat. Press 1 to agree to recording. Press 2 to continue without recording. If you do not choose, the call will continue without recording.</Say>\n  </Gather>\n</Response>",
        escape_xml(callback_url)
    )
}

pub fn redirect_xml(target: &str) -> String {
    format!(
        "{XML_DECLARATION}\n<Response>\n  <Redirect method=\"POST\">{}</Redirect>\n</Response>",
        escape_xml(target)
    )
}

fn escape_xml(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            '\t'
            | '\n'
            | '\r'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => escaped.push(character),
            _ => escaped.push('\u{fffd}'),
        }
    }
    escaped
}

fn trim_xml_space(value: &str) -> &str {
    value.trim_start_matches([' ', '\t', '\r', '\n'])
}

fn consume(remaining: &mut &str, expected: &str) -> SafeResult<()> {
    *remaining = trim_xml_space(remaining)
        .strip_prefix(expected)
        .ok_or("unexpected_upstream_twiml")?;
    Ok(())
}

fn valid_stream_url(value: &str) -> bool {
    // The native builder emits a WSS URL ending in crypto.randomUUID(). An
    // explicit grammar avoids interpreting entities, comments, or extra tags.
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':' | b'/' | b'[' | b']')
    }) {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    if url.scheme() != "wss"
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let Some(nonce) = url.path().rsplit('/').next() else {
        return false;
    };
    let bytes = nonce.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(offset, byte)| {
            if matches!(offset, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && bytes[14] == b'4'
        && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B')
}

/// Add recording only to the native realtime builder's single-stream response.
///
/// This is a shape check, not call authorization. Call only on the trusted
/// upstream response after a valid consent nonce and upstream policy approval.
/// Preserve its stream token byte-for-byte; never cache this short-lived XML.
pub fn insert_recording(xml: &str, callback_url: &str) -> SafeResult<String> {
    if xml.len() > MAX_FORM_BYTES {
        return Err("upstream_twiml_too_large");
    }
    let callback = reqwest::Url::parse(callback_url).map_err(|_| "invalid_recording_callback")?;
    if callback.scheme() != "https"
        || !callback.has_host()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
        || callback_url.chars().any(char::is_control)
    {
        return Err("invalid_recording_callback");
    }
    // Source of this grammar: OpenClaw RealtimeCallHandler.buildTwiMLPayload.
    let mut remaining = xml;
    consume(&mut remaining, XML_DECLARATION)?;
    consume(&mut remaining, "<Response>")?;
    let connect_offset = xml.len() - trim_xml_space(remaining).len();
    consume(&mut remaining, "<Connect>")?;
    consume(&mut remaining, "<Stream url=\"")?;
    let end_url = remaining.find('"').ok_or("unexpected_upstream_twiml")?;
    if !valid_stream_url(&remaining[..end_url]) {
        return Err("unexpected_upstream_stream_url");
    }
    remaining = &remaining[end_url..];
    consume(&mut remaining, "\" />")?;
    consume(&mut remaining, "</Connect>")?;
    consume(&mut remaining, "</Response>")?;
    if !trim_xml_space(remaining).is_empty() {
        return Err("unexpected_upstream_twiml");
    }
    let recording = format!(
        "<Start><Recording track=\"both\" channels=\"dual\" trim=\"do-not-trim\" recordingStatusCallback=\"{}\" recordingStatusCallbackMethod=\"POST\" recordingStatusCallbackEvent=\"completed absent\"/></Start>\n  ",
        escape_xml(callback_url)
    );
    let mut output = String::with_capacity(xml.len() + recording.len());
    output.push_str(&xml[..connect_offset]);
    output.push_str(&recording);
    output.push_str(&xml[connect_offset..]);
    Ok(output)
}

/// Hermetic checks for startup diagnostics; no credentials, network, or files.
pub fn self_test() -> SafeResult<()> {
    let form = parse_form(b"B=z&B=a&B=z&A=alpha%2Bbeta&Note=caf%C3%A9&Space=x+y")?;
    if one(&form, "A") != Some("alpha+beta")
        || one(&form, "Space") != Some("x y")
        || one(&form, "Note") != Some("caf\u{e9}")
        || one(&form, "B").is_some()
        || form.get("B").map(Vec::len) != Some(3)
    {
        return Err("self_test_form_values");
    }
    for invalid in [b"bad=%".as_slice(), b"bad=%GG", b"bad=%C3%28", b"bad=\xff"] {
        if parse_form(invalid).is_ok() {
            return Err("self_test_form_rejection");
        }
    }
    if parse_form(&vec![b'x'; MAX_FORM_BYTES + 1]).is_ok() {
        return Err("self_test_form_limit");
    }
    let url = "https://example.invalid/voice?nonce=alpha";
    let token = "synthetic-test-key-not-a-credential";
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, token.as_bytes());
    let expected_payload = format!("{url}Aalpha+betaBaBzNotecaf\u{e9}Spacex y");
    let signature = STANDARD.encode(hmac::sign(&key, expected_payload.as_bytes()).as_ref());
    let reordered = parse_form(b"Space=x+y&Note=caf%C3%A9&A=alpha%2Bbeta&B=a&B=z")?;
    let mut tampered = form.clone();
    tampered.insert("A".to_owned(), vec!["changed".to_owned()]);
    if !verify_signature(url, &form, &signature, token)
        || !verify_signature(url, &reordered, &signature, token)
        || verify_signature(url, &tampered, &signature, token)
        || verify_signature(
            "https://example.invalid/voice?nonce=beta",
            &form,
            &signature,
            token,
        )
        || verify_signature(url, &form, "not-base64", token)
        || verify_signature(url, &form, &signature, "")
    {
        return Err("self_test_signature");
    }
    if !valid_sid("CA0123456789abcdef0123456789abcdef", "CA")
        || valid_sid("RE0123456789abcdef0123456789abcdef", "CA")
        || valid_sid("CA0123456789abcdef0123456789abcdeg", "CA")
        || valid_sid("CA0123456789abcdef0123456789abcdef0", "CA")
    {
        return Err("self_test_sid");
    }
    let native = format!(
        "{XML_DECLARATION}\n<Response>\n  <Connect>\n    <Stream url=\"wss://example.invalid/voice/stream/realtime/12345678-1234-4234-8234-123456789abc\" />\n  </Connect>\n</Response>"
    );
    let callback = "https://example.invalid/recordings?nonce=alpha&stage=complete";
    let recorded = insert_recording(&native, callback)?;
    if !recorded.contains("nonce=alpha&amp;stage=complete")
        || !recorded.contains("track=\"both\" channels=\"dual\" trim=\"do-not-trim\"")
        || !recorded.contains("</Start>\n  <Connect>")
        || recorded.matches("<Stream ").count() != 1
        || insert_recording(&recorded, callback).is_ok()
        || insert_recording(&format!("{native}<Response/>"), callback).is_ok()
        || insert_recording(
            &native.replace("<Connect>", "<Say>No</Say><Connect>"),
            callback,
        )
        .is_ok()
        || insert_recording(&native.replace("wss://", "https://"), callback).is_ok()
        || insert_recording(&native.replace("123456789abc", "invalid"), callback).is_ok()
        || insert_recording(&native.replace(" />", " bad=\"extra\" />"), callback).is_ok()
        || insert_recording(&native, "http://example.invalid/callback").is_ok()
    {
        return Err("self_test_twiml");
    }
    let special = "https://example.invalid/continue?nonce=alpha&literal=\"'<tag>";
    if !redirect_xml(special).contains("nonce=alpha&amp;literal=&quot;&apos;&lt;tag&gt;</Redirect>")
        || !consent_xml(special).contains("action=\"https://example.invalid/continue?nonce=alpha&amp;literal=&quot;&apos;&lt;tag&gt;\"")
        || !consent_xml(callback).contains("actionOnEmptyResult=\"true\"")
        || !consent_xml(callback).contains("Press 2 to continue without recording.")
    {
        return Err("self_test_xml_escape");
    }
    Ok(())
}
