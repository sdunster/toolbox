//! AWS SES implementation of [`crate::mail::Handler`].
//!
//! Uses **`aws-sdk-sesv2`**, not the older `aws-sdk-ses` — v2's `SendEmail` API is
//! the one that also supports raw MIME content (`EmailContent::raw`), which step 6
//! needs for attachments and hand-built threading headers. Using v1 here and
//! switching later would mean redoing this whole module.
//!
//! **On `Message-ID`**: SES ignores whatever `Message-ID` header a raw message
//! carries and assigns its own on acceptance, returned as `SendEmailOutput::message_id`
//! — a bare id, not an RFC 5322 header value. The form that actually ends up in
//! the header SES sends (and therefore the form an inbound reply's
//! `In-Reply-To`/`References` will contain) is `<{message_id}@{region}.amazonses.com>`
//! — see the [SES developer guide][1]. [`Mailer::send_raw`] constructs exactly
//! that string and returns it, so what's stored on `ticket_message.rfc_message_id`
//! is what step 7's `rfc_message_id-index` lookup will actually be given, not the
//! bare id SES's API response carries.
//!
//! The region half of that string is read from the resolved SDK config
//! (`Mailer::new`), never hardcoded — this project runs in `ap-southeast-2`
//! today, but nothing here should silently stop working if that ever changes.
//!
//! [1]: https://docs.aws.amazon.com/ses/latest/dg/send-email-formatting.html

use anyhow::{Context, Result, anyhow};
use aws_sdk_sesv2::Client;
use aws_sdk_sesv2::primitives::Blob;
use aws_sdk_sesv2::types::{
    Body, Content, Destination, EmailContent, Message, MessageTag, RawMessage,
};

use crate::mail;

pub struct Mailer {
    client: Client,
    /// Read from the resolved SDK config at construction time — see this
    /// module's doc comment for why this must never be hardcoded.
    region: String,
}

impl Mailer {
    pub async fn new() -> Self {
        let config = crate::aws_config_loader().load().await;
        let region = config.region().map(|r| r.to_string()).unwrap_or_else(|| {
            // No region resolved from the environment/profile/IMDS at
            // all — this would already break every other AWS call this
            // process makes, so it's a configuration bug, not a normal
            // runtime condition. Fall back to something recognizable in
            // logs rather than panicking here; `send_raw` still works,
            // it just returns a `Message-ID` naming this placeholder
            // instead of a real region.
            tracing::error!(
                "AWS region not resolved from SDK config; outbound Message-IDs will say \
                     'unknown-region'. Check AWS_REGION / the active profile."
            );
            "unknown-region".to_string()
        });
        Self {
            client: Client::new(&config),
            region,
        }
    }

    async fn send(&self, to: &str, subject: &str, body: Body) -> Result<()> {
        let to = mail::resolve_recipient(to);
        let destination = Destination::builder().to_addresses(to.clone()).build();
        let subject_content = Content::builder().data(subject).charset("UTF-8").build()?;
        let message = Message::builder()
            .subject(subject_content)
            .body(body)
            .build();
        let content = EmailContent::builder().simple(message).build();
        self.client
            .send_email()
            .from_email_address(mail::system_from())
            .destination(destination)
            .content(content)
            .reply_to_addresses(mail::system_reply_to().to_string())
            .send()
            .await
            .with_context(|| format!("failed to send email to {}", to))?;
        Ok(())
    }
}

impl mail::Handler for Mailer {
    async fn send_plain_text(&self, to: &str, subject: &str, content: &str) -> Result<()> {
        let text = Content::builder().data(content).charset("UTF-8").build()?;
        self.send(to, subject, Body::builder().text(text).build())
            .await
    }

    async fn send_html(&self, to: &str, subject: &str, html: &str) -> Result<()> {
        let html = Content::builder().data(html).charset("UTF-8").build()?;
        self.send(to, subject, Body::builder().html(html).build())
            .await
    }

    async fn send_raw(&self, raw: &[u8], to: &[String], cc: &[String]) -> Result<String> {
        self.send_raw_tagged(raw, to, cc, &[]).await
    }

    async fn send_raw_tagged(
        &self,
        raw: &[u8],
        to: &[String],
        cc: &[String],
        tags: &[(String, String)],
    ) -> Result<String> {
        let to: Vec<String> = to.iter().map(|t| mail::resolve_recipient(t)).collect();
        let cc: Vec<String> = cc.iter().map(|t| mail::resolve_recipient(t)).collect();

        let mut destination = Destination::builder();
        for addr in &to {
            destination = destination.to_addresses(addr.clone());
        }
        for addr in &cc {
            destination = destination.cc_addresses(addr.clone());
        }
        let destination = destination.build();

        let raw_message = RawMessage::builder()
            .data(Blob::new(raw.to_vec()))
            .build()
            .context("building SES RawMessage")?;
        let content = EmailContent::builder().raw(raw_message).build();

        let mut request = self
            .client
            .send_email()
            .destination(destination)
            .content(content);
        for (name, value) in tags {
            request = request.email_tags(
                MessageTag::builder()
                    .name(name)
                    .value(value)
                    .build()
                    .context("building SES MessageTag")?,
            );
        }
        let resp = request
            .send()
            .await
            .with_context(|| format!("failed to send raw email to {to:?} cc {cc:?}"))?;

        let message_id = resp
            .message_id()
            .ok_or_else(|| anyhow!("SES send_email (raw) returned no message id"))?;
        Ok(format!("<{message_id}@{}.amazonses.com>", self.region))
    }
}
