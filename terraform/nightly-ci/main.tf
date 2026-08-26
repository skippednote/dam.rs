# The identity the nightly Glacier conformance run authenticates as.
#
# Least privilege derived from the driver rather than guessed: the fifteen S3 calls below are the ones
# `dam-store`'s S3 driver actually makes, read out of the source. A policy assembled from memory ends
# up as `s3:*` on a bucket, which is the same permission with a longer spelling.

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 6.0"
    }
  }
}

variable "bucket" {
  description = "The throwaway bucket the conformance suite writes to. Nothing else should use it."
  type        = string
}

variable "user_name" {
  description = "IAM user name for the nightly runner."
  type        = string
  default     = "damrs-nightly-conformance"
}

resource "aws_iam_user" "nightly" {
  name = var.user_name

  tags = {
    Purpose   = "dam.rs nightly S3/Glacier conformance suite"
    ManagedBy = "terraform/nightly-ci"
  }
}

# Object operations, scoped to the objects in one bucket.
#
# `RestoreObject` is the reason this identity exists at all — it is the call no local S3 server and no
# fake can answer, and the only one that proves the archival path against the storage a reader would
# actually use. The object-lock and retention calls are here because the shared conformance suite
# exercises them where the backend declares support; without them those cases fail as permission
# errors and read like capability bugs.
data "aws_iam_policy_document" "nightly" {
  statement {
    sid    = "ConformanceObjects"
    effect = "Allow"
    actions = [
      "s3:PutObject",
      "s3:GetObject",
      "s3:DeleteObject",
      "s3:RestoreObject",
      "s3:AbortMultipartUpload",
      "s3:ListMultipartUploadParts",
      "s3:GetObjectAttributes",
      "s3:PutObjectRetention",
      "s3:GetObjectRetention",
      "s3:PutObjectLegalHold",
      "s3:GetObjectLegalHold",
      "s3:BypassGovernanceRetention",
    ]
    resources = ["arn:aws:s3:::${var.bucket}/*"]
  }

  # Bucket-level, and separate because the resource ARN is the bucket rather than its contents — a
  # single statement covering both is the mistake that turns a scoped policy into a wide one.
  statement {
    sid    = "ConformanceBucket"
    effect = "Allow"
    actions = [
      "s3:ListBucket",
      "s3:ListBucketMultipartUploads",
      "s3:GetBucketLocation",
    ]
    resources = ["arn:aws:s3:::${var.bucket}"]
  }
}

resource "aws_iam_user_policy" "nightly" {
  name   = "${var.user_name}-s3-conformance"
  user   = aws_iam_user.nightly.name
  policy = data.aws_iam_policy_document.nightly.json
}

# No `aws_iam_access_key` here, and that is deliberate — see this module's README. The secret would
# land in Terraform state, which is a file people commit and push to buckets, and it would stay there
# for longer than the key does.

output "user_name" {
  description = "Create the access key against this user, then set the two repository secrets."
  value       = aws_iam_user.nightly.name
}

output "next_steps" {
  value = <<-EOT
    aws iam create-access-key --user-name ${aws_iam_user.nightly.name}
    gh secret set AWS_ACCESS_KEY_ID
    gh secret set AWS_SECRET_ACCESS_KEY
    gh secret set DAMRS_TEST_BUCKET
    # then uncomment `schedule:` in .github/workflows/nightly-aws.yml
  EOT
}
