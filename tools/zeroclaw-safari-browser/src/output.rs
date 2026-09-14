//! Page observations are disposable; operation and verification evidence is not.
//! Safari remains the source of truth. These previews add no cache or replay.
use anyhow::{Context, Result};
use serde_json::{Value, json};

// The daemon pretty-prints the complete MCP result, then embeds that string in
// native tool history with two further JSON escaping layers. Leave room for
// IDs, receipts and a second ordinary tool result under the 64 KiB round cap.
const ENCODED_RESULT_BYTES: usize = 24 * 1024;
const NOTICE: &str = "Page observation shortened for output size. Omitted text, controls and options are not evidence of absence. Scroll or wait for a specific observed selector to inspect another part of the page. Do not repeat an action because its page observation was shortened.";

fn envelope(result: &Value) -> Result<Value> {
    Ok(
        json!({"content":[{"type":"text","text":serde_json::to_string(result)?}],
        "structuredContent":result}),
    )
}

fn fits(result: &Value) -> Result<bool> {
    let mcp = envelope(result)?;
    let pretty = serde_json::to_string_pretty(&mcp)?;
    let once = serde_json::to_string(&pretty)?;
    let twice = serde_json::to_vec(&once)?;
    Ok(twice.len() <= ENCODED_RESULT_BYTES)
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = text.len().min(bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn shorten_text(page: &mut Value, key: &str, marker: &str, bytes: usize) {
    if let Some(text) = page[key].as_str()
        && text.len() > bytes
    {
        page[key] = json!(prefix(text, bytes));
        page[marker] = json!(true);
    }
}

fn page_mut<'a>(result: &'a mut Value, path: &str) -> Result<&'a mut Value> {
    result
        .pointer_mut(path)
        .context("Safari page observation disappeared during formatting")
}

fn controls_mut(page: &mut Value) -> Result<&mut Vec<Value>> {
    page.get_mut("controls")
        .and_then(Value::as_array_mut)
        .context("Safari page controls disappeared during formatting")
}

/// This is deliberately not a generic structured-result truncator. Only the
/// reader's page field in its two known return shapes may be shortened. All
/// action/applied/completed/readiness/verification/persistence fields survive.
pub fn format_result(mut result: Value, focus: Option<&str>) -> Result<Value> {
    let path = if result.get("page").is_some_and(Value::is_object) {
        "/page"
    } else if result.pointer("/state/page").is_some_and(Value::is_object) {
        "/state/page"
    } else {
        return envelope(&result);
    };
    if fits(&result)? {
        return envelope(&result);
    }

    // Take ownership rather than duplicating the original observation. The
    // reader already sorts viewport controls first; a requested exact control
    // takes priority so a dense page cannot crowd it out with unrelated links.
    let mut page = page_mut(&mut result, path)?.take();
    let mut controls = page
        .get_mut("controls")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .unwrap_or_default();
    if let Some(focus) = focus {
        controls.sort_by_key(|control| control["selector"].as_str() != Some(focus));
    }
    page["controls"] = json!([]);
    page["controlsTruncated"] = json!(true);
    page["outputTruncated"] = json!(true);
    page["outputNotice"] = json!(NOTICE);
    shorten_text(
        &mut page,
        "text",
        "textTruncated",
        if controls.is_empty() { 4096 } else { 1024 },
    );
    shorten_text(&mut page, "title", "titleTruncated", 256);
    *page_mut(&mut result, path)? = page;

    // Oversized exact page URLs are omitted as a whole, never shortened into a
    // different destination. Readiness and operation evidence remain untouched.
    if !fits(&result)? {
        let page = page_mut(&mut result, path)?;
        shorten_text(page, "text", "textTruncated", 0);
        if let Some(object) = page.as_object_mut()
            && object.remove("url").is_some()
        {
            page["urlOmitted"] = json!(true);
        }
    }

    for mut control in controls {
        // Preserve each option's exact selector/value. Remove whole trailing
        // options only, with the existing total and explicit omission flag.
        if let Some(options) = control.get_mut("options").and_then(Value::as_array_mut)
            && options.len() > 8
        {
            options.truncate(8);
            control["optionsTruncated"] = json!(true);
        }
        controls_mut(page_mut(&mut result, path)?)?.push(control);
        while !fits(&result)? {
            let controls = controls_mut(page_mut(&mut result, path)?)?;
            let candidate = controls
                .last_mut()
                .context("Safari candidate control disappeared during formatting")?;
            if let Some(options) = candidate.get_mut("options").and_then(Value::as_array_mut)
                && !options.is_empty()
            {
                options.pop();
                candidate["optionsTruncated"] = json!(true);
            } else {
                controls.pop();
                break;
            }
        }
    }
    let page = page_mut(&mut result, path)?;
    let retained = controls_mut(page)?.len();
    if page["totalControls"].as_u64() == Some(retained as u64) {
        page["controlsTruncated"] = json!(false);
    }
    // Unknown or oversized evidence outside the page keeps the existing exact
    // runtime admission behavior. Never erase a completed action to force an
    // arbitrary structured result under a presentation limit.
    envelope(&result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str, controls: Vec<Value>) -> Value {
        json!({"untrusted_web_content":true,"readiness":{"status":"ready","observed":{"ready":true}},
            "page":{"url":"https://example.com/form","title":"Offline form","text":text,
            "totalControls":controls.len(),"controlsTruncated":false,"controls":controls}})
    }

    fn check_bound(mcp: &Value) {
        let structured = &mcp["structuredContent"];
        assert_eq!(
            serde_json::from_str::<Value>(mcp["content"][0]["text"].as_str().unwrap()).unwrap(),
            *structured
        );
        assert!(fits(structured).unwrap());
        assert!(
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":mcp}))
                .unwrap()
                .len()
                < ENCODED_RESULT_BYTES
        );
        let output = serde_json::to_string_pretty(mcp).unwrap();
        let content = json!({"tool_call_id":"fixture-call","content":format!("{output}\n\n[receipt: fixture]")}).to_string();
        let message = json!({"role":"tool","content":content});
        assert!(serde_json::to_vec(&message).unwrap().len() < 25 * 1024);
        assert!(
            serde_json::to_vec(&json!([message, message]))
                .unwrap()
                .len()
                < 64 * 1024
        );
    }

    #[test]
    fn fitting_results_and_non_page_evidence_are_exact() {
        for result in [
            page("small", vec![]),
            json!({"action":"close","completed":true}),
            json!({"action":"verify","verification":{"exact":"x".repeat(50000)}}),
        ] {
            assert_eq!(
                format_result(result.clone(), None).unwrap(),
                envelope(&result).unwrap()
            );
        }
    }

    #[test]
    fn dense_unicode_controls_retain_exact_focus_and_report_omissions() {
        let controls: Vec<_> = (0..160).map(|i| json!({"selector":format!("shadow:[\"#host\",\"#field-{i}\"]"),
            "tag":"select","text":"Name 😀 \\\"".repeat(20), "inViewport":true,"hasValue":false,
            "totalOptions":250,"optionsTruncated":false,
            "options":(0..250).map(|n| json!({"selector":format!("#option-{i}-{n}"),"value":format!("exact-{n}"),"label":"😀\\\"".repeat(60)})).collect::<Vec<_>>()})).collect();
        let focus = controls[159]["selector"].as_str().unwrap().to_owned();
        let result =
            format_result(page(&"😀\\\"\n".repeat(10000), controls), Some(&focus)).unwrap();
        check_bound(&result);
        let page = &result["structuredContent"]["page"];
        assert_eq!(page["controls"][0]["selector"], focus);
        assert!(page["controls"].as_array().unwrap().len() > 1);
        assert_eq!(page["totalControls"], 160);
        assert_eq!(page["controlsTruncated"], true);
        assert_eq!(page["textTruncated"], true);
        assert_eq!(page["controls"][0]["optionsTruncated"], true);
        assert_eq!(page["controls"][0]["options"][0]["value"], "exact-0");
    }

    #[test]
    fn successful_and_unverified_actions_keep_exact_evidence() {
        for action in [
            "open", "verify", "fill", "select", "click", "check", "autofill",
        ] {
            for completed in [true, false] {
                let mut original = json!({"action":action,"applied":true,"completed":completed,
                    "verification":{"status":if completed {"ready"} else {"timed_out"},"observed":{"matches":completed,"evidence":"value"}},
                    "persistence":"not_verified","next_step":"Save and reopen before claiming persistence",
                    "state":page(&"😀\\\"\n".repeat(50000),vec![])});
                let result = format_result(original.clone(), None).unwrap();
                check_bound(&result);
                let mut actual = result["structuredContent"].clone();
                actual["state"]["page"] = Value::Null;
                original["state"]["page"] = Value::Null;
                assert_eq!(actual, original);
            }
        }
    }

    #[test]
    fn enormous_control_is_omitted_without_corrupting_its_selector() {
        let controls = vec![
            json!({"selector":"#exact","href":"x".repeat(100000)}),
            json!({"selector":"#usable","text":"Continue"}),
        ];
        let result = format_result(page("text", controls), None).unwrap();
        check_bound(&result);
        assert_eq!(
            result["structuredContent"]["page"]["controls"],
            json!([{"selector":"#usable","text":"Continue"}])
        );
        assert_eq!(
            result["structuredContent"]["page"]["controlsTruncated"],
            true
        );
    }
}
