//! In-process implementation of [`crate::mail::Handler`] that logs the message
//! instead of sending it.
//!
//! This is what makes the email-code login flow (`requestAuthCode`) usable with
//! no AWS account: the code is printed in the API's own log, so it can be pasted
//! straight into the browser. Set `MOCK_MAIL_DIR` to also drop each message into
//! a file there — handy for HTML emails, which are unreadable in a log line.

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};

use crate::mail;

/// Directory to additionally write each message to, one file per message.
pub const DIR_VAR: &str = "MOCK_MAIL_DIR";

/// Fake region tag for the synthetic `<...@...amazonses.com>` message ids
/// `send_raw` returns — see [`Handler::record_raw`]'s doc comment. Not a
/// real AWS region: the mock never talks to AWS, so it has no SDK config to
/// read one from, unlike `sesmail::Mailer`.
const MOCK_REGION: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email {
    pub to: String,
    pub subject: String,
    pub body: String,
    pub html: bool,
}

/// One `send_raw` call, as recorded by the mock. Carries the *resolved*
/// envelope recipients (after [`mail::resolve_recipient`]/`MAIL_OVERRIDE_TO`
/// have been applied — see [`Handler::record_raw`]) rather than whatever the
/// raw bytes' own headers say, mirroring how a real send's `Destination`
/// would differ from them under an override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawEmail {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub raw: Vec<u8>,
    pub message_id: String,
    /// Provider message tags, from [`mail::Handler::send_raw_tagged`]
    /// (empty for a plain `send_raw`).
    pub tags: Vec<(String, String)>,
}

#[derive(Default)]
pub struct Handler {
    sent: Mutex<Vec<Email>>,
    sent_raw: Mutex<Vec<RawEmail>>,
    dir: Option<PathBuf>,
}

impl Handler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Honours [`DIR_VAR`]; falls back to logging only.
    pub fn from_env() -> Self {
        let dir = std::env::var_os(DIR_VAR)
            .map(PathBuf::from)
            .filter(|d| !d.as_os_str().is_empty());
        if let Some(dir) = &dir {
            tracing::info!("mock mail: also writing messages to {}", dir.display());
        }
        Self {
            sent: Mutex::new(Vec::new()),
            sent_raw: Mutex::new(Vec::new()),
            dir,
        }
    }

    /// Every plain-text/HTML message sent so far (via `send_plain_text`/
    /// `send_html`), oldest first.
    pub fn sent(&self) -> Vec<Email> {
        self.sent.lock().expect("mockmail lock").clone()
    }

    /// Every raw message sent so far (via `send_raw` — ticket mail), oldest
    /// first.
    pub fn sent_raw(&self) -> Vec<RawEmail> {
        self.sent_raw.lock().expect("mockmail lock").clone()
    }

    pub fn clear(&self) {
        self.sent.lock().expect("mockmail lock").clear();
        self.sent_raw.lock().expect("mockmail lock").clear();
    }

    fn record(&self, to: &str, subject: &str, body: &str, html: bool) -> Result<()> {
        let to = mail::resolve_recipient(to);
        tracing::info!(
            "mock mail to {to}: {subject}\n--- begin message ---\n{body}\n--- end message ---"
        );
        let email = Email {
            to,
            subject: subject.to_string(),
            body: body.to_string(),
            html,
        };
        // One lock for the whole append, so the filename index and the vector
        // position can't disagree when two sends race.
        let mut sent = self.sent.lock().expect("mockmail lock");
        if let Some(dir) = &self.dir {
            // Best-effort would hide a typo'd MOCK_MAIL_DIR, and nothing here is on a
            // path where failing to write is better than saying so.
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            let ext = if html { "html" } else { "txt" };
            let path = dir.join(format!("{:04}-{}.{ext}", sent.len(), slug(subject)));
            std::fs::write(
                &path,
                format!(
                    "To: {}\nSubject: {}\n\n{}",
                    email.to, email.subject, email.body
                ),
            )
            .with_context(|| format!("writing {}", path.display()))?;
        }
        sent.push(email);
        Ok(())
    }

    /// [`mail::Handler::send_raw`]'s implementation. Applies
    /// [`mail::resolve_recipient`] to every envelope recipient — same as
    /// `record` does for `to` — so `MAIL_OVERRIDE_TO` redirects a raw send
    /// exactly as it redirects a plain-text/HTML one; only the *envelope*
    /// recipients are redirected, not the raw bytes' own `To`/`Cc` headers
    /// (see `mail::Handler::send_raw`'s doc comment for why that's the right
    /// layer), so a `.eml` written under `MOCK_MAIL_DIR` still shows the
    /// original intended recipients for debugging.
    fn record_raw(
        &self,
        raw: &[u8],
        to: &[String],
        cc: &[String],
        tags: &[(String, String)],
    ) -> Result<String> {
        let to: Vec<String> = to.iter().map(|t| mail::resolve_recipient(t)).collect();
        let cc: Vec<String> = cc.iter().map(|t| mail::resolve_recipient(t)).collect();
        tracing::info!(
            "mock mail (raw) to {to:?} cc {cc:?}: {} byte message",
            raw.len()
        );

        let message_id = format!("<mock-{}@{MOCK_REGION}.amazonses.com>", nanoid::nanoid!(16));

        let mut sent_raw = self.sent_raw.lock().expect("mockmail lock");
        if let Some(dir) = &self.dir {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            // Parsed back out of the raw bytes just built, purely to make a
            // readable filename — never used for anything that affects
            // behaviour, so a message that (somehow) fails to parse still
            // gets written under a generic name rather than losing the
            // `.eml` entirely.
            let subject = mail_parser::MessageParser::new()
                .parse(raw)
                .and_then(|m| m.subject().map(str::to_string))
                .unwrap_or_else(|| "no-subject".to_string());
            let path = dir.join(format!("{:04}-{}.eml", sent_raw.len(), slug(&subject)));
            std::fs::write(&path, raw).with_context(|| format!("writing {}", path.display()))?;
        }
        sent_raw.push(RawEmail {
            to,
            cc,
            raw: raw.to_vec(),
            message_id: message_id.clone(),
            tags: tags.to_vec(),
        });
        Ok(message_id)
    }
}

/// Filesystem-safe fragment of a subject line, for the per-message filename.
fn slug(subject: &str) -> String {
    let s: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let s = s.trim_matches('-').to_lowercase();
    s.chars().take(40).collect()
}

impl mail::Handler for Handler {
    async fn send_plain_text(&self, to: &str, subject: &str, content: &str) -> Result<()> {
        self.record(to, subject, content, false)
    }

    async fn send_html(&self, to: &str, subject: &str, html: &str) -> Result<()> {
        self.record(to, subject, html, true)
    }

    async fn send_raw(&self, raw: &[u8], to: &[String], cc: &[String]) -> Result<String> {
        self.record_raw(raw, to, cc, &[])
    }

    async fn send_raw_tagged(
        &self,
        raw: &[u8],
        to: &[String],
        cc: &[String],
        tags: &[(String, String)],
    ) -> Result<String> {
        self.record_raw(raw, to, cc, tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::Handler as _;

    #[tokio::test]
    async fn records_what_was_sent() {
        // Serialized against `mail::tests`, which mutate the same process-global
        // `MAIL_OVERRIDE_TO` this test implicitly asserts is *not* set — see
        // `mail::OVERRIDE_TO_ENV_LOCK`'s doc comment.
        let _guard = crate::mail::OVERRIDE_TO_ENV_LOCK.lock().await;

        let m = Handler::new();
        m.send_plain_text("a@example.com", "Your Toolbox login code", "123456")
            .await
            .unwrap();
        assert_eq!(
            m.sent(),
            vec![Email {
                to: "a@example.com".into(),
                subject: "Your Toolbox login code".into(),
                body: "123456".into(),
                html: false,
            }]
        );
    }

    #[tokio::test]
    async fn send_raw_records_the_envelope_recipients_and_returns_a_synthetic_message_id() {
        let _guard = crate::mail::OVERRIDE_TO_ENV_LOCK.lock().await;
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var(crate::mail::OVERRIDE_TO_VAR);
        }

        let m = Handler::new();
        let raw = b"From: a@example.com\r\nTo: b@example.com\r\n\r\nhello".to_vec();
        let to = vec!["b@example.com".to_string()];
        let cc = vec!["c@example.com".to_string()];
        let message_id = m.send_raw(&raw, &to, &cc).await.unwrap();

        assert!(
            message_id.starts_with('<') && message_id.ends_with(".amazonses.com>"),
            "message id must be in the <...@...amazonses.com> shape: {message_id:?}"
        );

        let sent = m.sent_raw();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, to);
        assert_eq!(sent[0].cc, cc);
        assert_eq!(sent[0].raw, raw);
        assert_eq!(sent[0].message_id, message_id);
    }

    #[tokio::test]
    async fn send_raw_honours_mail_override_to() {
        let _guard = crate::mail::OVERRIDE_TO_ENV_LOCK.lock().await;
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::set_var(crate::mail::OVERRIDE_TO_VAR, "override@example.com");
        }

        let m = Handler::new();
        let raw = b"From: a@example.com\r\n\r\nhello".to_vec();
        m.send_raw(
            &raw,
            &["real-requester@example.com".to_string()],
            &["real-cc@example.com".to_string()],
        )
        .await
        .unwrap();

        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var(crate::mail::OVERRIDE_TO_VAR);
        }

        let sent = m.sent_raw();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].to, vec!["override@example.com".to_string()]);
        assert_eq!(sent[0].cc, vec!["override@example.com".to_string()]);
    }

    #[tokio::test]
    async fn send_raw_writes_an_eml_file_under_mock_mail_dir() {
        let _guard = crate::mail::OVERRIDE_TO_ENV_LOCK.lock().await;
        // SAFETY: serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var(crate::mail::OVERRIDE_TO_VAR);
        }

        let dir = std::env::temp_dir().join(format!("toolbox-mockmail-{}", nanoid::nanoid!(8)));
        let m = Handler {
            sent: Mutex::new(Vec::new()),
            sent_raw: Mutex::new(Vec::new()),
            dir: Some(dir.clone()),
        };
        let raw = b"From: a@example.com\r\nSubject: [#acme-1] hi\r\n\r\nbody".to_vec();
        m.send_raw(&raw, &["b@example.com".to_string()], &[])
            .await
            .unwrap();

        let entries: Vec<_> = std::fs::read_dir(&dir)
            .expect("dir written")
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(
            entries[0].extension().is_some_and(|e| e == "eml"),
            "{entries:?}"
        );
        let written = std::fs::read(&entries[0]).unwrap();
        assert_eq!(
            written, raw,
            "the .eml file must contain the raw bytes verbatim"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn slug_is_filename_safe() {
        let s = slug("[#acme-42] New reply");
        assert!(
            s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "unexpected characters in {s:?}"
        );
        assert!(s.starts_with("acme-42"), "{s:?}");
    }
}
