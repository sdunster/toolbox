# GitHub Actions OIDC deploy role. CI assumes this via web identity
# federation — no long-lived access keys stored anywhere. Scoped to exactly
# what deploy-prod.yml needs: update the two lambdas' code, sync the web
# bucket, invalidate CloudFront.
#
# No iam:PassRole (cargo lambda deploy never passes a role — the functions
# already have theirs, Terraform-managed in iam.tf) and no
# lambda:CreateFunction (Terraform, applied by hand, owns creation; CI only
# ever updates an existing function's code).

resource "aws_iam_openid_connect_provider" "github" {
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
  # GitHub's OIDC provider thumbprint (root CA), current at time of writing.
  # See https://github.blog/changelog/2023-06-27-github-actions-update-on-oidc-integration-with-aws/ —
  # AWS itself no longer validates this against the fingerprint (it's now
  # validated against a fixed set of trusted CAs internally), but the
  # provider resource still requires a value here.
  thumbprint_list = ["6938fd4d98bab03faadb97b34396831e3780aea1"]
}

resource "aws_iam_role" "github_deploy" {
  name = "toolbox-github-deploy"

  # Tighter than the seslogin pattern this was copied from: the sub
  # condition pins to a push landing ON the prod branch specifically
  # (`ref:refs/heads/prod`), not `repo:...:*`. A PR against prod, a tag, or
  # any other branch cannot assume this role — only deploy-prod.yml's own
  # trigger can.
  #
  # Two accepted subjects, because GitHub issues the claim in two shapes and
  # which one you get is a repository setting, not something the workflow
  # controls:
  #
  #   repo:owner/name:ref:refs/heads/prod                    (name-based)
  #   repo:owner@<owner_id>/name@<repo_id>:ref:refs/heads/prod  (immutable)
  #
  # The immutable form embeds GitHub's numeric owner and repository ids, so
  # trust does not survive a repo being deleted and recreated under the same
  # name — strictly better, and worth preferring where available. StringEquals
  # against a list matches if ANY element matches, so listing both keeps the
  # deploy working whichever shape the repo is configured to emit, without
  # resorting to a wildcard that would widen what can assume this role.
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Federated = aws_iam_openid_connect_provider.github.arn }
      Action    = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = {
          "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
          "token.actions.githubusercontent.com:sub" = compact([
            "repo:${var.github_repo}:ref:refs/heads/prod",
            var.github_repo_immutable == "" ? "" : "repo:${var.github_repo_immutable}:ref:refs/heads/prod",
          ])
        }
      }
    }]
  })
}

resource "aws_iam_role_policy" "github_lambda_deploy" {
  name = "lambda-deploy"
  role = aws_iam_role.github_deploy.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect = "Allow"
      Action = [
        "lambda:GetFunction",
        "lambda:UpdateFunctionCode",
      ]
      Resource = [
        aws_lambda_function.api.arn,
        aws_lambda_function.worker.arn,
      ]
    }]
  })
}

resource "aws_iam_role_policy" "github_s3_deploy" {
  name = "s3-web-deploy"
  role = aws_iam_role.github_deploy.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["s3:ListBucket"]
        Resource = [aws_s3_bucket.web.arn]
      },
      {
        Effect   = "Allow"
        Action   = ["s3:PutObject", "s3:DeleteObject", "s3:GetObject"]
        Resource = ["${aws_s3_bucket.web.arn}/*"]
      },
    ]
  })
}

resource "aws_iam_role_policy" "github_cloudfront_invalidate" {
  name = "cloudfront-invalidate"
  role = aws_iam_role.github_deploy.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect   = "Allow"
      Action   = ["cloudfront:CreateInvalidation"]
      Resource = [aws_cloudfront_distribution.web.arn]
    }]
  })
}
