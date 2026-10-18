# The email-assets bucket: images an HTML broadcast shows, served publicly
# by the web CloudFront distribution under /email-assets/* (web.tf) so every
# recipient's mail client can fetch them from the web app's own domain.
#
# Kept apart from the mail bucket on purpose: that one holds customer mail
# and must never be reachable without a presigned URL. This one is public by
# design — never put anything private in it.
#
# Key layout (api/src/graphql/mutations.rs, createBroadcastAssetUpload):
#   email-assets/{instance_id}/{nanoid}/{filename}
#                       one uploaded image, write-once by presigned PUT from
#                       the browser. No lifecycle rule: mail already sent
#                       must keep rendering, so nothing here ever expires.
#
# The key doubles as the URL path, so CloudFront needs no origin path or
# rewrite: https://{web_domain}/email-assets/... is s3://.../email-assets/...

resource "aws_s3_bucket" "email_assets" {
  bucket = "toolbox-email-assets-${var.aws_account_id}"
}

# Still fully private at the S3 level: public reads go through CloudFront's
# origin access control, never a public bucket policy or ACL.
resource "aws_s3_bucket_public_access_block" "email_assets" {
  bucket = aws_s3_bucket.email_assets.id

  block_public_acls       = true
  ignore_public_acls      = true
  block_public_policy     = true
  restrict_public_buckets = true
}

# Same shape as the web bucket's policy: only this distribution may read.
resource "aws_s3_bucket_policy" "email_assets" {
  bucket = aws_s3_bucket.email_assets.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "AllowCloudFrontOAC"
      Effect    = "Allow"
      Principal = { Service = "cloudfront.amazonaws.com" }
      Action    = "s3:GetObject"
      Resource  = "${aws_s3_bucket.email_assets.arn}/*"
      Condition = {
        StringEquals = {
          "AWS:SourceArn" = aws_cloudfront_distribution.web.arn
        }
      }
    }]
  })
}

# The browser PUTs each image straight to its presigned URL, cross-origin
# from the web app.
resource "aws_s3_bucket_cors_configuration" "email_assets" {
  bucket = aws_s3_bucket.email_assets.id

  cors_rule {
    allowed_methods = ["PUT"]
    allowed_origins = distinct(concat(["https://${var.web_domain}"], var.allowed_origins))
    allowed_headers = ["Content-Type"]
    max_age_seconds = 3000
  }
}
