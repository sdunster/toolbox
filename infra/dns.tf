# DNS is deliberately NOT managed here.
#
# The hosted zone for this deployment's parent domain lives in a different AWS
# account from the one this stack is applied to, so Terraform has no way to
# write records into it. Rather than pretend otherwise -- a `data` lookup of a
# zone in another account simply fails -- this file computes every record that
# has to exist and exposes them as an output for whoever administers that zone
# to create by hand.
#
# The consequence to plan around: `aws_acm_certificate_validation` in acm.tf
# blocks until the certificate's validation record is live, so the first apply
# is a two-phase affair. See DEVELOPMENT.md.
#
# If the zone ever moves into this account, replace this file with ordinary
# `aws_route53_record` resources and a `data "aws_route53_zone"` lookup; the
# values below are already exactly what those records need to contain.

locals {
  # CloudFront's fixed, global hosted-zone id -- the same for every
  # distribution in every account. Only needed if the records below are
  # created as Route53 alias records rather than plain CNAMEs.
  cloudfront_alias_zone_id = "Z2FDTNDATAQYW2"

  # One DMARC policy for every sending domain. Started at p=none with rua
  # reports to alert_email; the reports came back clean (SES's Easy DKIM
  # aligns on the header From domain), so this is now p=reject with no rua --
  # aggregate reports were only needed to confirm that, and are noise
  # thereafter. To investigate a deliverability problem, temporarily add
  # "; rua=mailto:<address>" here and re-create the records.
  dmarc_record = "v=DMARC1; p=reject"

  # SPF for every sending domain and custom MAIL FROM subdomain. All mail goes
  # out through SES, so -all (hard fail) excludes nothing legitimate.
  spf_record = "v=spf1 include:amazonses.com -all"

  dns_records_required = concat(
    [
      {
        name    = var.web_domain
        type    = "A / AAAA (alias) or CNAME"
        value   = aws_cloudfront_distribution.web.domain_name
        purpose = "Serves the web app. An alias record in Route53 (zone ${local.cloudfront_alias_zone_id}), or a plain CNAME in any other DNS provider."
      },
      {
        name    = var.support_domain
        type    = "MX"
        value   = "10 inbound-smtp.${var.aws_region}.amazonaws.com"
        purpose = "Delivers inbound mail to SES. Without this, nothing reaches the ticket system."
      },
      {
        name    = var.support_domain
        type    = "TXT"
        value   = local.spf_record
        purpose = "SPF: authorises SES to send as this domain."
      },
      {
        name    = "_dmarc.${var.support_domain}"
        type    = "TXT"
        value   = local.dmarc_record
        purpose = "DMARC: reject mail that fails both SPF and DKIM alignment."
      },
      {
        name    = aws_sesv2_email_identity_mail_from_attributes.main.mail_from_domain
        type    = "MX"
        value   = "10 feedback-smtp.${var.aws_region}.amazonses.com"
        purpose = "Custom MAIL FROM: bounce and complaint feedback."
      },
      {
        name    = aws_sesv2_email_identity_mail_from_attributes.main.mail_from_domain
        type    = "TXT"
        value   = local.spf_record
        purpose = "SPF for the custom MAIL FROM subdomain."
      },
    ],
    # The web domain's system-mail sending identity (login codes) — SPF/DMARC/
    # DKIM/MAIL FROM only, no inbound MX: this domain never receives ticket
    # mail. Empty when web_domain == support_domain (no separate identity
    # exists in that case — see ses.tf).
    length(aws_sesv2_email_identity.system) == 0 ? [] : concat(
      [
        {
          name    = var.web_domain
          type    = "TXT"
          value   = local.spf_record
          purpose = "SPF: authorises SES to send system mail (login codes) as this domain."
        },
        {
          name    = "_dmarc.${var.web_domain}"
          type    = "TXT"
          value   = local.dmarc_record
          purpose = "DMARC for the web domain's system mail."
        },
        {
          name    = aws_sesv2_email_identity_mail_from_attributes.system[0].mail_from_domain
          type    = "MX"
          value   = "10 feedback-smtp.${var.aws_region}.amazonses.com"
          purpose = "Custom MAIL FROM: bounce and complaint feedback for system mail."
        },
        {
          name    = aws_sesv2_email_identity_mail_from_attributes.system[0].mail_from_domain
          type    = "TXT"
          value   = local.spf_record
          purpose = "SPF for the custom MAIL FROM subdomain."
        },
      ],
      [
        for token in aws_sesv2_email_identity.system[0].dkim_signing_attributes[0].tokens : {
          name    = "${token}._domainkey.${var.web_domain}"
          type    = "CNAME"
          value   = "${token}.dkim.amazonses.com"
          purpose = "DKIM signing for system mail. All three must exist before SES will sign it."
        }
      ]
    ),
    # Additional mail domains: the same MX/SPF/DMARC/MAIL FROM/DKIM set as the
    # primary domain. If the domain already has an MX or SPF record, creating
    # these REPLACES it -- check before creating.
    flatten([
      for d, id in aws_sesv2_email_identity.additional : concat(
        [
          {
            name    = d
            type    = "MX"
            value   = "10 inbound-smtp.${var.aws_region}.amazonaws.com"
            purpose = "Delivers inbound mail for ${d} to SES."
          },
          {
            name    = d
            type    = "TXT"
            value   = local.spf_record
            purpose = "SPF for ${d}."
          },
          {
            name    = "_dmarc.${d}"
            type    = "TXT"
            value   = local.dmarc_record
            purpose = "DMARC for ${d}."
          },
          {
            name    = aws_sesv2_email_identity_mail_from_attributes.additional[d].mail_from_domain
            type    = "MX"
            value   = "10 feedback-smtp.${var.aws_region}.amazonses.com"
            purpose = "Custom MAIL FROM for ${d}."
          },
          {
            name    = aws_sesv2_email_identity_mail_from_attributes.additional[d].mail_from_domain
            type    = "TXT"
            value   = local.spf_record
            purpose = "SPF for the custom MAIL FROM subdomain of ${d}."
          },
        ],
        [
          for token in id.dkim_signing_attributes[0].tokens : {
            name    = "${token}._domainkey.${d}"
            type    = "CNAME"
            value   = "${token}.dkim.amazonses.com"
            purpose = "DKIM signing for ${d}."
          }
        ]
      )
    ]),
    # DKIM: three CNAMEs, generated by SES when the identity is created.
    [
      for token in aws_sesv2_email_identity.main.dkim_signing_attributes[0].tokens : {
        name    = "${token}._domainkey.${var.support_domain}"
        type    = "CNAME"
        value   = "${token}.dkim.amazonses.com"
        purpose = "DKIM signing. All three must exist before SES will sign outbound mail."
      }
    ],
    # ACM validation: the certificate cannot issue, and therefore CloudFront
    # cannot serve TLS, until this exists.
    [
      for opt in aws_acm_certificate.web.domain_validation_options : {
        name    = opt.resource_record_name
        type    = opt.resource_record_type
        value   = opt.resource_record_value
        purpose = "ACM certificate validation. The first apply BLOCKS on this -- create it, then re-apply."
      }
    ],
  )
}

output "dns_records_required" {
  description = "Every DNS record that must be created by hand in the parent zone (which lives in another account). Create the ACM validation record first: the initial apply blocks until the certificate issues."
  value       = local.dns_records_required
}
