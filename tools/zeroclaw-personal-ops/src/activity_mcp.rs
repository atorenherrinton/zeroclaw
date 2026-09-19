//! Bounded read projections of the canonical operations ledger. Full dashboard,
//! background alerts and exact outbox reviews retain their existing owners.
use crate::{Ops, digest};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};

const BUDGET: usize = 12 * 1024;
const SECTIONS: &[&str] = &[
    "operations",
    "receipts",
    "projects",
    "shipments",
    "sources",
    "health",
    "pending_events",
    "legacy_messages",
];
const EXPECTED_SOURCES: &[&str] = &[
    "calendar_today",
    "important_email",
    "pending_invitations",
    "overdue_reminders",
    "scheduled_jobs",
    "github",
];

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Request {
    section: Option<String>,
    source: Option<String>,
    pointer: Option<String>,
    revision: Option<String>,
    offset: usize,
    limit: Option<usize>,
    refresh: bool,
}

pub fn parameters() -> Value {
    json!({
        "section":{"type":"string","enum":SECTIONS},
        "source":{"type":"string","maxLength":256,"description":"Inspect this source snapshot instead of the overview; read only."},
        "pointer":{"type":"string","maxLength":2048,"description":"JSON pointer in the source snapshot. Empty selects its root."},
        "revision":{"type":"string","maxLength":64,"description":"Use the returned revision for consistent source pagination; changed snapshots fail explicitly."},
        "offset":{"type":"integer","minimum":0,"description":"Next offset returned by the previous page."},
        "limit":{"type":"integer","minimum":1,"maximum":50,"description":"Maximum records or source children per page; encoded size may reduce it."}
    })
}

// Measure MCP's JSON-in-text plus an additional encoded history envelope. This
// standalone helper intentionally does not depend on the runtime crate or change
// its authoritative per-result and aggregate budgets.
fn encoded_size(value: &Value) -> Result<usize> {
    let mcp = json!({"content":[{"type":"text","text":value.to_string()}],"isError":false});
    let output = format!(
        "{}\n{}",
        serde_json::to_string_pretty(&mcp)?,
        "r".repeat(128)
    );
    let content = json!({"tool_call_id":"c".repeat(128),"content":output}).to_string();
    Ok(serde_json::to_vec(&json!({"role":"tool","content":content}))?.len())
}

fn bounded(value: Value) -> Result<Value> {
    ensure!(
        encoded_size(&value)? <= BUDGET,
        "activity page metadata exceeds output budget; narrow the request"
    );
    Ok(value)
}

fn preview(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(s) if s.chars().count() > 80 => json!({
            "preview":s.chars().take(80).collect::<String>(), "omitted":true,
            "characters":s.chars().count()
        }),
        Value::Array(a) => json!({"count":a.len(), "preview":if depth == 0 {vec![]} else {
            a.iter().take(2).map(|v|preview(v, depth-1)).collect::<Vec<_>>()
        }, "omitted":depth == 0 && !a.is_empty() || a.len() > 2}),
        Value::Object(o) => {
            if depth == 0 {
                return json!({"field_count":o.len(),"omitted":!o.is_empty()});
            }
            let mut result = serde_json::Map::new();
            for (k, v) in o.iter().take(8) {
                // Do not emit arbitrary huge metadata keys into a bounded overview.
                if k.len() <= 128 {
                    result.insert(k.clone(), preview(v, depth - 1));
                }
            }
            json!({"preview":result,"field_count":o.len(),"omitted":true})
        }
        _ => value.clone(),
    }
}

fn select(value: &Value, keys: &[&str]) -> Value {
    let mut result = serde_json::Map::new();
    for key in keys {
        let identity = matches!(
            *key,
            "operation_id" | "project_id" | "shipment_id" | "draft_id" | "id" | "source" | "name"
        );
        result.insert(
            (*key).into(),
            if identity {
                value[*key].clone()
            } else {
                preview(&value[*key], 1)
            },
        );
    }
    Value::Object(result)
}

fn summarize(section: &str, row: &Value) -> Value {
    let mut result = match section {
        "operations" => {
            let mut v = select(
                row,
                &[
                    "operation_id",
                    "state",
                    "created_ms",
                    "authorized_ms",
                    "send_at_ms",
                ],
            );
            v["title"] = preview(&row["review"]["title"], 0);
            let mut states = serde_json::Map::new();
            for step in row["steps"].as_array().into_iter().flatten() {
                if let Some(state) = step["state"].as_str() {
                    let count = states.get(state).and_then(Value::as_u64).unwrap_or(0);
                    states.insert(state.into(), json!(count + 1));
                }
            }
            v["step_states"] = json!(states);
            v["details"] = json!({"tool":"outbox_status", "operation_id":row["operation_id"],"exact_review_required_before_send":true});
            v
        }
        "receipts" => {
            let mut v = select(row, &["sequence", "id", "ordinal", "state", "created_ms"]);
            v["evidence"] = preview(&row["evidence"], 1);
            v["details"] = json!(
                "Full evidence is retained in the authenticated activity dashboard; receipt IDs may identify operations, projects or shipments."
            );
            v
        }
        "projects" => {
            let mut v = select(row, &["project_id", "revision", "updated_ms"]);
            v["project"] = select(
                &row["project"],
                &[
                    "status",
                    "desired_outcome",
                    "next_action",
                    "blocker",
                    "waiting_on",
                    "deadline",
                ],
            );
            v
        }
        "shipments" => select(
            row,
            &[
                "shipment_id",
                "carrier",
                "label",
                "state",
                "expected_at",
                "updated_ms",
                "tracking_url",
            ],
        ),
        "sources" => {
            let mut v = select(row, &["source", "verified_ms", "stale", "error"]);
            v["available"] = json!(row["verified_ms"].as_i64().is_some_and(|n| n > 0));
            v["data"] = preview(&row["data"], 1);
            v["source_truncated"] = json!(
                row["data"]["truncated"] == true || row["data"].get("nextPageToken").is_some()
            );
            v["details"] = json!({"source":row["source"],"pointer":""});
            v
        }
        "health" => select(
            row,
            &[
                "name",
                "state",
                "consecutive_failures",
                "last_checked_ms",
                "last_success_ms",
                "detail",
            ],
        ),
        "pending_events" => select(
            row,
            &["id", "source", "kind", "state", "attempts", "last_error"],
        ),
        _ => select(
            row,
            &[
                "draft_id",
                "state",
                "send_at_ms",
                "approved_ms",
                "attempted_ms",
            ],
        ),
    };
    result["summary_only"] = json!(true);
    result
}

pub(crate) fn validate(args: &Value, briefing: bool) -> Result<()> {
    parse_request(args, briefing).map(|_| ())
}

fn parse_request(args: &Value, briefing: bool) -> Result<(Request, usize)> {
    let request: Request = serde_json::from_value(args.clone())
        .map_err(|_| anyhow::Error::msg("invalid activity arguments"))?;
    let limit = request
        .limit
        .unwrap_or(if request.section.is_some() || request.source.is_some() {
            10
        } else {
            2
        });
    ensure!(
        (1..=50).contains(&limit) && request.offset <= i64::MAX as usize,
        "invalid activity page bounds"
    );
    ensure!(
        !request.refresh || briefing,
        "refresh is only supported by personal_briefing"
    );
    ensure!(
        request.section.is_none() || request.source.is_none(),
        "choose section or source, not both"
    );
    ensure!(
        request.section.is_some() || request.source.is_some() || request.offset == 0,
        "offset requires section or source"
    );
    ensure!(
        request.source.is_some() || (request.pointer.is_none() && request.revision.is_none()),
        "pointer and revision require source"
    );
    Ok((request, limit))
}

impl Ops {
    pub fn activity_mcp(&self, args: &Value, briefing: bool) -> Result<Value> {
        let (request, limit) = parse_request(args, briefing)?;
        // A read transaction keeps all overview counts/pages from one SQLite view.
        let transaction = self.db.unchecked_transaction()?;
        let result = if let Some(source) = request.source.as_deref() {
            self.source_page(source, &request, limit)?
        } else {
            if let Some(section) = request.section.as_deref() {
                ensure!(SECTIONS.contains(&section), "unknown activity section");
            }
            let sections = request
                .section
                .as_deref()
                .map_or_else(|| SECTIONS.to_vec(), |s| vec![s]);
            let mut output = json!({"generated_at":Utc::now().to_rfc3339(),"summary_only":true,
                "presentation":if briefing {"Exception-focused briefing. Do not infer empty sources from missing, stale or omitted data."} else {"Read-only activity overview. Omitted reviews are not authorization or proof that an action did not occur."},
                "pagination":if briefing {"Continue with personal_briefing using section and next_offset, refresh=false. Section totals count briefing matches; source/pointer/revision inspect snapshots."} else {"Continue with operations_activity using section and next_offset; source/pointer/revision inspect snapshots. Pages reflect current ledger state and may shift when records change."},"sections":{}});
            if request.section.is_none() {
                let names = self
                    .db
                    .prepare("SELECT source FROM source_snapshots")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                output["missing_sources"] = json!(
                    EXPECTED_SOURCES
                        .iter()
                        .filter(|s| !names.iter().any(|n| n == **s))
                        .collect::<Vec<_>>()
                );
            }
            for section in sections {
                let (total, rows) = self.activity_rows(
                    section,
                    request.offset,
                    if section == "sources" && request.section.is_none() && request.limit.is_none()
                    {
                        50
                    } else {
                        limit
                    },
                    briefing,
                )?;
                let mut page = json!({"total":total,"offset":request.offset,"items":[],"next_offset":null,"omitted":false});
                let mut items = Vec::new();
                for row in rows {
                    let mut item = summarize(section, &row);
                    if encoded_size(&item)? > BUDGET - 2048 {
                        // A single large evidence/project projection must not
                        // strand section pagination on the same row forever.
                        item = select(
                            &row,
                            &[
                                "operation_id",
                                "project_id",
                                "shipment_id",
                                "draft_id",
                                "id",
                                "source",
                                "name",
                                "sequence",
                                "state",
                                "verified_ms",
                                "stale",
                                "error",
                            ],
                        );
                        item["project_status"] = row["project"]["status"].clone();
                        item["summary_only"] = json!(true);
                        item["details_omitted"] = json!(true);
                        item["warning"] = json!(
                            "Large descriptive fields omitted; exact source data and the authenticated dashboard retain details."
                        );
                        ensure!(
                            encoded_size(&item)? <= BUDGET - 2048,
                            "activity record identity exceeds output budget"
                        );
                    }
                    items.push(item);
                    page["items"] = json!(items);
                    page["next_offset"] = json!(request.offset + items.len());
                    output["sections"][section] = page.clone();
                    // Reserve enough metadata for the remaining overview sections.
                    let ceiling = if request.section.is_some() {
                        BUDGET - 512
                    } else if section == "sources" {
                        5000
                    } else {
                        2500
                    };
                    if encoded_size(&page)? > ceiling {
                        items.pop();
                        break;
                    }
                }
                let next = request.offset + items.len();
                page["items"] = json!(items);
                page["omitted"] = json!(next < total);
                page["next_offset"] = if next < total && !items.is_empty() {
                    json!(next)
                } else {
                    Value::Null
                };
                if items.is_empty() && request.offset < total {
                    page["warning"] = json!(
                        "Record summary exceeds this overview allowance; request this section alone with limit=1."
                    );
                }
                output["sections"][section] = page;
            }
            // Defer whole rows only; every omitted summary stays pageable.
            while encoded_size(&output)? > BUDGET {
                let section = SECTIONS
                    .iter()
                    .filter(|s| {
                        output["sections"][**s]["items"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty())
                    })
                    .max_by_key(|s| output["sections"][**s]["items"].to_string().len())
                    .context("activity overview metadata exceeds output budget")?;
                let page = &mut output["sections"][*section];
                let items = page["items"].as_array_mut().context("activity items")?;
                items.pop();
                let returned = items.len();
                page["omitted"] = json!(true);
                page["next_offset"] = if returned > 0 {
                    json!(request.offset + returned)
                } else {
                    Value::Null
                };
                page["warning"] =
                    json!("Some summaries were deferred to fit; request this section alone.");
            }
            bounded(output)?
        };
        transaction.commit()?;
        Ok(result)
    }

    fn activity_rows(
        &self,
        section: &str,
        offset: usize,
        limit: usize,
        briefing: bool,
    ) -> Result<(usize, Vec<Value>)> {
        if briefing && section == "operations" {
            let ids = self
                .db
                .prepare("SELECT id FROM operations ORDER BY created_ms DESC,id")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut total = 0;
            let mut rows = Vec::new();
            for id in ids {
                let row = self.operation_status(&id)?;
                if matches!(
                    row["state"].as_str(),
                    Some("failed" | "uncertain" | "partial")
                ) {
                    if total >= offset && rows.len() < limit {
                        rows.push(row);
                    }
                    total += 1;
                }
            }
            return Ok((total, rows));
        }
        let (table, query) = match (briefing, section) {
            (true, "projects") => (
                "projects WHERE json_extract(payload,'$.status') NOT IN ('completed','paused')",
                "SELECT json_object('project_id',id,'revision',revision,'project',json(payload),'updated_ms',updated_ms) FROM projects WHERE json_extract(payload,'$.status') NOT IN ('completed','paused') ORDER BY updated_ms DESC,id LIMIT ?1 OFFSET ?2",
            ),
            (true, "shipments") => (
                "shipments WHERE state IN ('delayed','exception','out_for_delivery')",
                "SELECT json_object('shipment_id',id,'carrier',carrier,'tracking_number',tracking_number,'label',label,'state',state,'expected_at',expected_at,'evidence',json(evidence),'updated_ms',updated_ms) FROM shipments WHERE state IN ('delayed','exception','out_for_delivery') ORDER BY updated_ms DESC,id LIMIT ?1 OFFSET ?2",
            ),
            (true, "health") => (
                "connector_health WHERE state != 'healthy'",
                "SELECT json_object('name',name,'state',state,'consecutive_failures',consecutive_failures,'last_checked_ms',last_checked_ms,'last_success_ms',last_success_ms,'detail',detail) FROM connector_health WHERE state != 'healthy' ORDER BY name LIMIT ?1 OFFSET ?2",
            ),
            (_, "operations") => (
                "operations",
                "SELECT id FROM operations ORDER BY created_ms DESC,id LIMIT ?1 OFFSET ?2",
            ),
            (_, "receipts") => (
                "operation_receipts",
                "SELECT json_object('sequence',sequence,'id',operation_id,'ordinal',ordinal,'state',state,'evidence',json(evidence),'created_ms',created_ms) FROM operation_receipts ORDER BY sequence DESC LIMIT ?1 OFFSET ?2",
            ),
            (_, "projects") => (
                "projects",
                "SELECT json_object('project_id',id,'revision',revision,'project',json(payload),'updated_ms',updated_ms) FROM projects ORDER BY updated_ms DESC,id LIMIT ?1 OFFSET ?2",
            ),
            (_, "shipments") => (
                "shipments",
                "SELECT json_object('shipment_id',id,'carrier',carrier,'tracking_number',tracking_number,'label',label,'state',state,'expected_at',expected_at,'evidence',json(evidence),'updated_ms',updated_ms) FROM shipments ORDER BY updated_ms DESC,id LIMIT ?1 OFFSET ?2",
            ),
            (_, "sources") => (
                "source_snapshots",
                "SELECT json_object('source',source,'data',json(payload),'verified_ms',verified_ms,'error',error) FROM source_snapshots ORDER BY error IS NULL,verified_ms,source LIMIT ?1 OFFSET ?2",
            ),
            (_, "health") => (
                "connector_health",
                "SELECT json_object('name',name,'state',state,'consecutive_failures',consecutive_failures,'last_checked_ms',last_checked_ms,'last_success_ms',last_success_ms,'detail',detail) FROM connector_health ORDER BY state='healthy',name LIMIT ?1 OFFSET ?2",
            ),
            (_, "pending_events") => (
                "event_inbox WHERE state!='done'",
                "SELECT json_object('id',id,'source',source,'kind',kind,'state',state,'attempts',attempts,'last_error',last_error) FROM event_inbox WHERE state!='done' ORDER BY created_ms,id LIMIT ?1 OFFSET ?2",
            ),
            (_, "legacy_messages") => (
                "imessage_queue",
                "SELECT json_object('draft_id',plan_id,'state',state,'send_at_ms',send_at_ms,'approved_ms',approved_ms,'attempted_ms',attempted_ms) FROM imessage_queue ORDER BY plan_id LIMIT ?1 OFFSET ?2",
            ),
            _ => anyhow::bail!("unknown activity section"),
        };
        // Both SQL fragments come only from the fixed match above.
        let total: usize =
            self.db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        let raw = self
            .db
            .prepare(query)?
            .query_map(params![limit as i64, offset as i64], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::new();
        for raw in raw {
            let mut row: Value = if section == "operations" {
                self.operation_status(&raw)?
            } else {
                serde_json::from_str(&raw)?
            };
            if section == "sources" {
                row["stale"] = json!(
                    Utc::now().timestamp_millis()
                        - row["verified_ms"]
                            .as_i64()
                            .context("invalid source timestamp")?
                        > 30 * 60_000
                );
            }
            if section == "shipments" {
                row["tracking_url"] = json!(crate::continuity::tracking_url(
                    row["carrier"].as_str().context("invalid carrier")?,
                    row["tracking_number"]
                        .as_str()
                        .context("invalid tracking number")?
                ));
            }
            rows.push(row);
        }
        Ok((total, rows))
    }

    fn source_page(&self, source: &str, request: &Request, limit: usize) -> Result<Value> {
        ensure!(!source.is_empty() && source.len() <= 256, "invalid source");
        ensure!(
            request.offset == 0 || request.revision.is_some(),
            "source continuation requires revision from the previous page"
        );
        let pointer = request.pointer.as_deref().unwrap_or("");
        ensure!(
            pointer.len() <= 2048 && (pointer.is_empty() || pointer.starts_with('/')),
            "invalid source pointer"
        );
        let (payload, at, error): (String, i64, Option<String>) = self
            .db
            .query_row(
                "SELECT payload,verified_ms,error FROM source_snapshots WHERE source=?1",
                [source],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .context("source unavailable: no snapshot recorded")?;
        let revision =
            digest(format!("{at}\n{payload}\n{}", error.as_deref().unwrap_or("")).as_bytes());
        ensure!(
            request.revision.as_ref().is_none_or(|r| r == &revision),
            "source snapshot changed; restart pagination without revision"
        );
        let data: Value = serde_json::from_str(&payload)?;
        let value = data.pointer(pointer).context("source pointer not found")?;
        let mut page = json!({"source":source,"pointer":pointer,"revision":revision,"verified_ms":at,
            "available":at > 0,"stale":Utc::now().timestamp_millis() - at > 30 * 60_000,
            "error":preview(&json!(error),0),"offset":request.offset,"next_offset":null,
            "source_truncated":data["truncated"] == true || data.get("nextPageToken").is_some(),
            "read_only":true,"omitted":false});
        if let Some(s) = value.as_str() {
            // String offsets are Unicode scalar positions, never split UTF-8.
            let total = s.chars().count();
            ensure!(
                request.offset <= total,
                "source offset exceeds string length"
            );
            let mut chunk: String = s.chars().skip(request.offset).take(1024).collect();
            loop {
                let next = request.offset + chunk.chars().count();
                page["text"] = json!(chunk);
                page["total_characters"] = json!(total);
                page["next_offset"] = if next < total {
                    json!(next)
                } else {
                    Value::Null
                };
                page["omitted"] = json!(next < total || request.offset > 0);
                if encoded_size(&page)? <= BUDGET {
                    break;
                }
                ensure!(!chunk.is_empty(), "source metadata exceeds output budget");
                chunk.truncate(
                    chunk
                        .char_indices()
                        .nth(chunk.chars().count() / 2)
                        .map_or(0, |(i, _)| i),
                );
            }
        } else {
            let children: Vec<(String, &Value)> = match value {
                Value::Array(a) => a
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v))
                    .collect(),
                Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), v)).collect(),
                _ => {
                    ensure!(request.offset == 0, "scalar source offset must be zero");
                    page["value"] = value.clone();
                    return bounded(page);
                }
            };
            ensure!(
                request.offset <= children.len(),
                "source offset exceeds collection length"
            );
            let mut items = Vec::new();
            for (key, child) in children.iter().skip(request.offset).take(limit) {
                let child_pointer =
                    format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                let exact = encoded_size(child)? <= 1800;
                items.push(json!({"pointer":child_pointer,"complete":exact,"value":if exact {(*child).clone()} else {preview(child,1)}}));
                page["items"] = json!(items);
                // Reserve changing pagination metadata before accepting an item.
                if encoded_size(&page)? > BUDGET - 256 {
                    items.pop();
                    break;
                }
            }
            ensure!(
                !items.is_empty() || request.offset == children.len(),
                "source child metadata exceeds output budget; use a narrower pointer"
            );
            let next = request.offset + items.len();
            page["items"] = json!(items);
            page["total"] = json!(children.len());
            page["next_offset"] = if next < children.len() {
                json!(next)
            } else {
                Value::Null
            };
            page["omitted"] = json!(
                next < children.len()
                    || request.offset > 0
                    || page["items"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|i| i["complete"] != true)
            );
        }
        bounded(page)
    }
}
