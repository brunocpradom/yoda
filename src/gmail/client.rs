//! A thin async client over the Gmail REST API (`gmail.googleapis.com`). It
//! authenticates each call with a bearer token from [`super::auth`] (refreshed
//! transparently) and returns plain data the tools turn into model-readable
//! text. Kept deliberately small: search, read a thread, send, and trash/delete.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use super::auth;

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// One row of a search result.
pub struct ThreadSummary {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub snippet: String,
}

pub struct Gmail {
    http: reqwest::Client,
}

impl Gmail {
    pub fn new() -> Result<Gmail> {
        let http = reqwest::Client::builder()
            .user_agent("yoda/0.1 (+gmail)")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("could not build HTTP client")?;
        Ok(Gmail { http })
    }

    // --- read paths -----------------------------------------------------------

    /// Search threads with a Gmail query (e.g. `is:unread`, `from:x`), returning
    /// up to `max` summaries (subject/from/date/snippet of the latest message).
    pub async fn search(&self, query: &str, max: u32) -> Result<Vec<ThreadSummary>> {
        let list: Value = self
            .get(&format!(
                "{API}/threads?q={}&maxResults={max}",
                urlencode(query)
            ))
            .await?;
        let Some(threads) = list.get("threads").and_then(|t| t.as_array()) else {
            return Ok(Vec::new());
        };

        let mut out = Vec::new();
        for t in threads.iter().take(max as usize) {
            let Some(id) = t.get("id").and_then(Value::as_str) else {
                continue;
            };
            // Per-thread metadata fetch: threads.list omits headers, so pull the
            // latest message's From/Subject/Date plus its snippet.
            let meta: Value = self
                .get(&format!(
                    "{API}/threads/{id}?format=metadata\
                     &metadataHeaders=From&metadataHeaders=Subject&metadataHeaders=Date"
                ))
                .await?;
            let last = meta
                .get("messages")
                .and_then(Value::as_array)
                .and_then(|m| m.last());
            out.push(ThreadSummary {
                id: id.to_string(),
                from: header(last, "From"),
                subject: header(last, "Subject"),
                date: header(last, "Date"),
                snippet: last
                    .and_then(|m| m.get("snippet"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(out)
    }

    /// Fetch a whole thread as readable text: each message's From/Subject/Date
    /// header block followed by its decoded plain-text body.
    pub async fn read_thread(&self, thread_id: &str) -> Result<String> {
        let thread: Value = self
            .get(&format!("{API}/threads/{thread_id}?format=full"))
            .await?;
        let messages = thread
            .get("messages")
            .and_then(Value::as_array)
            .context("thread has no messages")?;

        let mut out = String::new();
        for (i, m) in messages.iter().enumerate() {
            if i > 0 {
                out.push_str("\n\n----------\n\n");
            }
            out.push_str(&format!(
                "From: {}\nDate: {}\nSubject: {}\n\n",
                header(Some(m), "From"),
                header(Some(m), "Date"),
                header(Some(m), "Subject"),
            ));
            out.push_str(&extract_body(m.get("payload")).unwrap_or_else(|| {
                m.get("snippet")
                    .and_then(Value::as_str)
                    .unwrap_or("(no readable body)")
                    .to_string()
            }));
        }
        Ok(out)
    }

    // --- write paths ----------------------------------------------------------

    /// Send a plain-text email. Returns the new message id.
    pub async fn send(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        cc: Option<&str>,
        bcc: Option<&str>,
    ) -> Result<String> {
        let raw = build_mime(to, subject, body, cc, bcc);
        let encoded = URL_SAFE_NO_PAD.encode(raw.as_bytes());
        let resp: Value = self
            .post(&format!("{API}/messages/send"), json!({ "raw": encoded }))
            .await?;
        Ok(resp
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("(unknown)")
            .to_string())
    }

    /// Move a thread to Trash (reversible).
    pub async fn trash_thread(&self, thread_id: &str) -> Result<()> {
        let _: Value = self
            .post(&format!("{API}/threads/{thread_id}/trash"), json!({}))
            .await?;
        Ok(())
    }

    /// Permanently delete a thread (irreversible — needs full-access scope).
    pub async fn delete_thread(&self, thread_id: &str) -> Result<()> {
        let token = auth::bearer().await?;
        let resp = self
            .http
            .delete(format!("{API}/threads/{thread_id}"))
            .bearer_auth(token)
            .send()
            .await
            .context("delete request failed")?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("Gmail delete returned {status}: {}", body);
        }
        Ok(())
    }

    // --- HTTP helpers ---------------------------------------------------------

    async fn get(&self, url: &str) -> Result<Value> {
        let token = auth::bearer().await?;
        let resp = self
            .http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .context("Gmail request failed")?;
        json_or_err(resp).await
    }

    async fn post(&self, url: &str, body: Value) -> Result<Value> {
        let token = auth::bearer().await?;
        let resp = self
            .http
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .context("Gmail request failed")?;
        json_or_err(resp).await
    }
}

async fn json_or_err(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        // Surface Gmail's own error message (e.g. permission/quota) verbatim.
        bail!("Gmail API returned {status}: {text}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).context("could not parse Gmail response")
}

// --- payload parsing ----------------------------------------------------------

/// Look up a header (case-insensitive) on a message's `payload.headers`.
fn header(message: Option<&Value>, name: &str) -> String {
    message
        .and_then(|m| m.get("payload"))
        .and_then(|p| p.get("headers"))
        .and_then(Value::as_array)
        .and_then(|hs| {
            hs.iter().find(|h| {
                h.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
        })
        .and_then(|h| h.get("value"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Walk a MIME payload tree and return the best readable text: prefer
/// `text/plain`, fall back to `text/html` (tags stripped).
fn extract_body(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    let mime = payload
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("");

    if mime == "text/plain"
        && let Some(text) = decode_part(payload)
    {
        return Some(text);
    }

    if let Some(parts) = payload.get("parts").and_then(Value::as_array) {
        // Prefer a plain-text part anywhere in the tree.
        for p in parts {
            if p.get("mimeType").and_then(Value::as_str) == Some("text/plain")
                && let Some(text) = decode_part(p)
            {
                return Some(text);
            }
        }
        // Then recurse (multipart/alternative, multipart/mixed, …).
        for p in parts {
            if let Some(text) = extract_body(Some(p)) {
                return Some(text);
            }
        }
    }

    if mime == "text/html"
        && let Some(html) = decode_part(payload)
    {
        return Some(html2text::from_read(html.as_bytes(), 100).unwrap_or(html));
    }
    None
}

/// Decode a single part's base64url `body.data` into a UTF-8 string.
fn decode_part(part: &Value) -> Option<String> {
    let data = part.get("body")?.get("data")?.as_str()?;
    let cleaned: String = data.chars().filter(|c| !c.is_whitespace()).collect();
    // Gmail uses URL-safe base64; tolerate presence or absence of padding.
    let bytes = URL_SAFE
        .decode(&cleaned)
        .or_else(|_| URL_SAFE_NO_PAD.decode(cleaned.trim_end_matches('=')))
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

// --- outgoing message assembly ------------------------------------------------

/// Build a minimal RFC 5322 message. A non-ASCII Subject is RFC 2047 encoded so
/// accented headers (e.g. Portuguese) survive; the UTF-8 body rides inside the
/// base64url `raw` field untouched.
fn build_mime(to: &str, subject: &str, body: &str, cc: Option<&str>, bcc: Option<&str>) -> String {
    let mut msg = String::new();
    msg.push_str(&format!("To: {to}\r\n"));
    if let Some(cc) = cc.filter(|s| !s.is_empty()) {
        msg.push_str(&format!("Cc: {cc}\r\n"));
    }
    if let Some(bcc) = bcc.filter(|s| !s.is_empty()) {
        msg.push_str(&format!("Bcc: {bcc}\r\n"));
    }
    msg.push_str(&format!("Subject: {}\r\n", encode_header(subject)));
    msg.push_str("MIME-Version: 1.0\r\n");
    msg.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
    msg.push_str("Content-Transfer-Encoding: 8bit\r\n");
    msg.push_str("\r\n");
    msg.push_str(body);
    msg
}

fn encode_header(value: &str) -> String {
    if value.is_ascii() {
        value.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", STANDARD.encode(value.as_bytes()))
    }
}

fn urlencode(s: &str) -> String {
    // Percent-encode via the url parser so query operators in `q` survive intact.
    let mut u = reqwest::Url::parse("http://localhost/").expect("static URL");
    u.query_pairs_mut().append_pair("q", s);
    // u.query() is "q=<encoded>"; strip the "q=" prefix.
    u.query()
        .and_then(|q| q.strip_prefix("q="))
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let m = json!({"payload": {"headers": [
            {"name": "Subject", "value": "Hello"},
            {"name": "From", "value": "a@b.com"}
        ]}});
        assert_eq!(header(Some(&m), "subject"), "Hello");
        assert_eq!(header(Some(&m), "FROM"), "a@b.com");
        assert_eq!(header(Some(&m), "missing"), "");
    }

    #[test]
    fn decodes_base64url_plain_part() {
        let data = URL_SAFE_NO_PAD.encode(b"hello world");
        let part = json!({"body": {"data": data}});
        assert_eq!(decode_part(&part).as_deref(), Some("hello world"));
    }

    #[test]
    fn extracts_plain_from_multipart_alternative() {
        let plain = URL_SAFE_NO_PAD.encode(b"the text");
        let payload = json!({
            "mimeType": "multipart/alternative",
            "parts": [
                {"mimeType": "text/html", "body": {"data": URL_SAFE_NO_PAD.encode(b"<p>x</p>")}},
                {"mimeType": "text/plain", "body": {"data": plain}}
            ]
        });
        assert_eq!(extract_body(Some(&payload)).as_deref(), Some("the text"));
    }

    #[test]
    fn mime_has_headers_and_body() {
        let m = build_mime("a@b.com", "Hi", "Body line", None, None);
        assert!(m.contains("To: a@b.com\r\n"));
        assert!(m.contains("Subject: Hi\r\n"));
        assert!(m.contains("\r\n\r\nBody line"));
    }

    #[test]
    fn non_ascii_subject_is_rfc2047_encoded() {
        let m = build_mime("a@b.com", "Olá café", "x", None, None);
        assert!(m.contains("Subject: =?UTF-8?B?"));
    }

    #[test]
    fn cc_and_bcc_included_when_present() {
        let m = build_mime("a@b.com", "s", "b", Some("c@d.com"), Some("e@f.com"));
        assert!(m.contains("Cc: c@d.com\r\n"));
        assert!(m.contains("Bcc: e@f.com\r\n"));
    }

    #[test]
    fn urlencode_escapes_spaces_and_operators() {
        assert_eq!(
            urlencode("is:unread newer_than:1d"),
            "is%3Aunread+newer_than%3A1d"
        );
    }

    // Live end-to-end: exercises auth::bearer() + the REST search against the
    // real account. Needs a cached token (`yoda gmail-login`); run explicitly:
    //   cargo test --release gmail::client::tests::live_search_unread -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "hits the live Gmail API; needs ~/.yoda/gmail.json"]
    async fn live_search_unread() {
        let threads = Gmail::new()
            .unwrap()
            .search("is:unread", 3)
            .await
            .expect("search should succeed with a valid token");
        eprintln!("live search returned {} thread(s)", threads.len());
        for t in &threads {
            eprintln!("  [{}] {} — {}", t.id, t.from, t.subject);
        }
    }
}
