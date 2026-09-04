use crate::common::{SafeResult, check};
use base64::{Engine, engine::general_purpose::STANDARD};
use std::collections::BTreeMap;

pub type Form = BTreeMap<String, String>;

fn decode(s: &str) -> SafeResult<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                check(i + 2 < b.len(), "invalid_form_escape")?;
                let h = (b[i + 1] as char)
                    .to_digit(16)
                    .ok_or("invalid_form_escape")?;
                let l = (b[i + 2] as char)
                    .to_digit(16)
                    .ok_or("invalid_form_escape")?;
                out.push((h * 16 + l) as u8);
                i += 2;
            }
            v => out.push(v),
        }
        i += 1;
    }
    let value = String::from_utf8(out).map_err(|_| "invalid_form_utf8")?;
    check(!value.contains('\0'), "invalid_form_nul")?;
    Ok(value)
}

pub fn parse_form(bytes: &[u8]) -> SafeResult<Form> {
    check(bytes.len() <= 32 * 1024, "form_too_large")?;
    let raw = std::str::from_utf8(bytes).map_err(|_| "invalid_form_utf8")?;
    let mut out = Form::new();
    if raw.is_empty() {
        return Ok(out);
    }
    for pair in raw.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = decode(k)?;
        let v = decode(v)?;
        check(
            !k.is_empty() && !out.contains_key(&k),
            "ambiguous_form_field",
        )?;
        out.insert(k, v);
    }
    Ok(out)
}

pub fn one<'a>(form: &'a Form, key: &str) -> SafeResult<&'a str> {
    form.get(key)
        .map(String::as_str)
        .ok_or("missing_form_field")
}

pub fn valid_sid(s: &str, prefix: &str) -> bool {
    s.len() == 34 && s.starts_with(prefix) && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn signature_ok(token: &str, public_url: &str, form: &Form, signature: &str) -> bool {
    if token.is_empty() || signature.len() != 28 {
        return false;
    }
    let Ok(signature) = STANDARD.decode(signature) else {
        return false;
    };
    let mut message = public_url.as_bytes().to_vec();
    for (key, value) in form {
        message.extend(key.as_bytes());
        message.extend(value.as_bytes());
    }
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, token.as_bytes());
    ring::hmac::verify(&key, &message, &signature).is_ok()
}

pub fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Twilio documents an optional trailing slash in voice WebSocket signatures.
/// Only these four exact spellings of our configured, nonce-bound URL are valid.
pub fn websocket_signature_ok(token: &str, https_url: &str, signature: &str) -> bool {
    if !https_url.starts_with("https://") || https_url.contains('?') || https_url.contains('#') {
        return false;
    }
    let wss_url = https_url.replacen("https://", "wss://", 1);
    let empty = Form::new();
    [
        https_url.to_owned(),
        format!("{https_url}/"),
        wss_url.clone(),
        format!("{wss_url}/"),
    ]
    .iter()
    .any(|url| signature_ok(token, url, &empty, signature))
}

pub const EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response/>";
pub const REJECT: &str =
    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Reject reason=\"rejected\"/></Response>";

pub fn consent_xml(base: &str, nonce: &str) -> String {
    let action = xml(&format!("{base}/voice/consent/{nonce}"));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response><Gather input=\"dtmf\" numDigits=\"1\" timeout=\"10\" actionOnEmptyResult=\"true\" action=\"{action}\" method=\"POST\"><Say>You have reached an AI call-screening assistant. This call will be transcribed to take a message. With your permission, the conversation will also be recorded and privately sent to the person you called. Press 1 to agree to recording, or press 2 to continue without recording. If you do not want transcription, please hang up.</Say></Gather><Hangup/></Response>"
    )
}

pub fn connect_xml(base: &str, nonce: &str, record: bool) -> String {
    let stream = xml(&format!(
        "{}/voice/media/{nonce}",
        base.replacen("https://", "wss://", 1)
    ));
    let recording = if record {
        format!(
            "<Start><Recording track=\"both\" channels=\"dual\" trim=\"do-not-trim\" recordingStatusCallback=\"{}\" recordingStatusCallbackMethod=\"POST\" recordingStatusCallbackEvent=\"completed absent\"/></Start>",
            xml(&format!("{base}/voice/recording"))
        )
    } else {
        String::new()
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Response>{recording}<Connect><Stream url=\"{stream}\"/></Connect><Hangup/></Response>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn websocket_signatures_only_accept_exact_canonical_variants() {
        use base64::Engine;
        let target = "https://phone.example/voice/media/fixture-nonce";
        for spelling in [
            target.to_owned(),
            format!("{target}/"),
            target.replacen("https://", "wss://", 1),
            format!("{}/", target.replacen("https://", "wss://", 1)),
        ] {
            let key =
                ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, b"fixture-token");
            let signature = base64::engine::general_purpose::STANDARD
                .encode(ring::hmac::sign(&key, spelling.as_bytes()).as_ref());
            assert!(websocket_signature_ok("fixture-token", target, &signature));
            assert!(!websocket_signature_ok(
                "fixture-token",
                "https://other.example/voice/media/fixture-nonce",
                &signature
            ));
            assert!(!websocket_signature_ok(
                "fixture-token",
                "https://phone.example/voice/media/other-nonce",
                &signature
            ));
        }
        assert!(!websocket_signature_ok(
            "fixture-token",
            &format!("{target}?x=1"),
            "invalid"
        ));
    }
    #[test]
    fn parser_rejects_ambiguity_and_invalid_bytes() {
        for value in [
            "CallSid=a&CallSid=b",
            "a=%ZZ",
            "a=%FF",
            "a=%",
            "a=%00",
            "=x",
        ] {
            assert!(parse_form(value.as_bytes()).is_err());
        }
        assert_eq!(parse_form(b"x=a%2Bb+c").unwrap()["x"], "a+b c");
    }
    #[test]
    fn official_signature_fixture() {
        let f = parse_form(b"CallSid=CA1234567890ABCDE&Caller=%2B14158675310&Digits=1234&From=%2B14158675310&To=%2B18005551212").unwrap();
        assert!(signature_ok(
            "12345",
            "https://example.com/myapp.php?foo=1&bar=2",
            &f,
            "L/OH5YylLD5NRKLltdqwSvS0BnU="
        ));
        assert!(!signature_ok(
            "wrong",
            "https://example.com/myapp.php?foo=1&bar=2",
            &f,
            "L/OH5YylLD5NRKLltdqwSvS0BnU="
        ));
    }
    #[test]
    fn consent_precedes_and_controls_recording() {
        assert!(!consent_xml("https://test.invalid", "nonce").contains("<Recording"));
        assert!(!connect_xml("https://test.invalid", "nonce", false).contains("<Recording"));
        let xml = connect_xml("https://test.invalid", "nonce", true);
        assert!(xml.find("<Recording").unwrap() < xml.find("<Connect").unwrap());
        assert!(xml.contains("channels=\"dual\""));
    }
}
