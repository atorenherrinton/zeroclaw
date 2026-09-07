//! Single-attempt, receipt-checked text delivery. Edits retain the existing
//! message on failure. No timeout/5xx/malformed acknowledgement permits replay.
use super::*;
#[cfg(test)]
mod tests;
use sha2::{Digest, Sha256};
use zeroclaw_api::delivery::{ChunkReceipt, DeliveryFailure, EffectOutcome, current_journal};

struct SubmissionFailure {
    outcome: EffectOutcome,
    reformat: bool,
}
impl From<EffectOutcome> for SubmissionFailure {
    fn from(outcome: EffectOutcome) -> Self {
        Self {
            outcome,
            reformat: false,
        }
    }
}

impl TelegramChannel {
    async fn submit_text_chunk(
        &self,
        method: &str,
        body: &serde_json::Value,
        edit_id: Option<i64>,
    ) -> Result<String, SubmissionFailure> {
        let mut response = self
            .http_client()
            .post(self.api_url(method))
            .timeout(Duration::from_secs(30))
            .json(body)
            .send()
            .await
            .map_err(|_| EffectOutcome::PossiblyApplied)?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(part) = response
            .chunk()
            .await
            .map_err(|_| EffectOutcome::PossiblyApplied)?
        {
            if bytes.len().saturating_add(part.len()) > 64 * 1024 {
                return Err(EffectOutcome::ReconciliationRequired.into());
            }
            bytes.extend_from_slice(&part);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| EffectOutcome::ReconciliationRequired)?;
        if status.is_success() && value["ok"] == true {
            if let Some(id) = value["result"]["message_id"].as_i64().filter(|id| *id > 0) {
                if edit_id.is_some_and(|expected| expected != id) {
                    return Err(EffectOutcome::ReconciliationRequired.into());
                }
                return Ok(id.to_string());
            }
            return Err(EffectOutcome::ReconciliationRequired.into());
        }
        if status == reqwest::StatusCode::BAD_REQUEST && value["ok"] == false {
            return Err(SubmissionFailure {
                outcome: EffectOutcome::ConfirmedFailed,
                reformat: true,
            });
        }
        // Authentication and rate-limit rejections with a valid Bot API error
        // envelope prove nonacceptance; upstream 5xx and proxies do not.
        if matches!(status.as_u16(), 401 | 403 | 429) && value["ok"] == false {
            return Err(EffectOutcome::ConfirmedFailed.into());
        }
        Err(EffectOutcome::PossiblyApplied.into())
    }

    pub(super) async fn deliver_text_chunks(
        &self,
        text: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        draft_id: Option<i64>,
    ) -> anyhow::Result<()> {
        let result = self
            .deliver_text_chunks_inner(text, chat_id, thread_id, draft_id)
            .await;
        let summary = match &result {
            Ok(()) => zeroclaw_api::delivery::DeliverySummary {
                outcome: EffectOutcome::Confirmed,
                confirmed_chunks: split_message_for_telegram(text).len(),
                total_chunks: split_message_for_telegram(text).len(),
            },
            Err(error) => {
                let failure = error.downcast_ref::<DeliveryFailure>();
                zeroclaw_api::delivery::DeliverySummary {
                    outcome: failure.map_or(EffectOutcome::ReconciliationRequired, |f| f.outcome),
                    confirmed_chunks: failure.map_or(0, |f| f.confirmed_chunks),
                    total_chunks: failure.map_or(0, |f| f.total_chunks),
                }
            }
        };
        zeroclaw_api::delivery::record_summary(summary);
        result
    }

    async fn deliver_text_chunks_inner(
        &self,
        text: &str,
        chat_id: &str,
        thread_id: Option<&str>,
        draft_id: Option<i64>,
    ) -> anyhow::Result<()> {
        if draft_id.is_some_and(|id| id <= 0) {
            return Err(DeliveryFailure {
                outcome: EffectOutcome::NotStarted,
                chunk_index: 0,
                total_chunks: split_message_for_telegram(text).len(),
                confirmed_chunks: 0,
            }
            .into());
        }
        let chunks = split_message_for_telegram(text);
        let route = zeroclaw_api::conversation::current().filter(|r| {
            r.reply_to.parse::<i64>().is_ok_and(|id| id > 0)
                && r.channel == format!("telegram.{}", self.alias)
                && r.recipient.split(':').next() == Some(chat_id)
                && r.recipient
                    .split_once(':')
                    .map(|(_, t)| t)
                    .or(r.thread.as_deref())
                    == thread_id
        });
        // Structured tuple prevents delimiter collisions. Response content hash
        // identifies this exact immutable response, not a newly generated answer.
        let response_key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "telegram-text-v1",
                &self.alias,
                chat_id,
                thread_id,
                route.as_ref().map(|r| &r.reply_to),
                text,
            ))?)
        );
        // No stable inbound identity means no safe durable deduplication key.
        let journal = route.as_ref().and_then(|_| current_journal());
        let mut confirmed = 0;
        for (index, chunk) in chunks.iter().enumerate() {
            let fail = |outcome| DeliveryFailure {
                outcome,
                chunk_index: index,
                total_chunks: chunks.len(),
                confirmed_chunks: confirmed,
            };
            let mut receipt = ChunkReceipt {
                key: format!("{response_key}:{index}"),
                response_key: response_key.clone(),
                chunk_index: index,
                total_chunks: chunks.len(),
                outcome: EffectOutcome::PossiblyApplied,
                platform_message_id: None,
            };
            if let Some(journal) = &journal {
                match journal.claim(receipt.clone()).await {
                    Ok(Some(old)) if old.outcome == EffectOutcome::Confirmed => {
                        confirmed += 1;
                        continue;
                    }
                    Ok(Some(_)) => return Err(fail(EffectOutcome::ReconciliationRequired).into()),
                    Ok(None) => {}
                    Err(_) => return Err(fail(EffectOutcome::NotStarted).into()),
                }
            }
            let rendered = format_telegram_text_chunk(chunk, index, chunks.len());
            let edit_id = if index == 0 { draft_id } else { None };
            let method = if edit_id.is_some() {
                "editMessageText"
            } else {
                "sendMessage"
            };
            let mut body = serde_json::json!({"chat_id":chat_id,"text":Self::markdown_to_telegram_html(&rendered),"parse_mode":"HTML"});
            if let Some(id) = edit_id {
                body["message_id"] = id.into();
            } else {
                if let Some(tid) = thread_id {
                    body["message_thread_id"] = tid.into();
                }
                if index == 0
                    && let Some(id) = route.as_ref().and_then(|r| r.reply_to.parse::<i64>().ok())
                {
                    body["reply_parameters"] =
                        serde_json::json!({"message_id":id,"allow_sending_without_reply":true});
                }
            }
            let mut result = self.submit_text_chunk(method, &body, edit_id).await;
            // A definitive rejection permits a formatting-only attempt. No
            // retries of accepted or ambiguous sends, and never delete+send.
            if result.as_ref().is_err_and(|e| e.reformat) {
                if let Some(object) = body.as_object_mut() {
                    object.remove("parse_mode");
                }
                body["text"] = rendered.into();
                result = self.submit_text_chunk(method, &body, edit_id).await;
            }
            match &result {
                Ok(id) => {
                    receipt.outcome = EffectOutcome::Confirmed;
                    receipt.platform_message_id = Some(id.clone());
                }
                Err(error) => receipt.outcome = error.outcome,
            }
            if let Some(journal) = &journal
                && journal.finish(receipt.clone()).await.is_err()
            {
                // The write-ahead row remains possibly_applied after a disk
                // failure; successful network I/O alone is not durable success.
                return Err(fail(EffectOutcome::ReconciliationRequired).into());
            }
            ::zeroclaw_log::record!(
                INFO,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_attrs(serde_json::json!({"delivery_chunk":receipt})),
                "Telegram text chunk acknowledgement"
            );
            if let Err(error) = result {
                return Err(fail(error.outcome).into());
            }
            confirmed += 1;
        }
        Ok(())
    }
}
