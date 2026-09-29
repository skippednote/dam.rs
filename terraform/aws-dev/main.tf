terraform {
  required_version = ">= 1.6"
  required_providers {
    aws    = { source = "hashicorp/aws", version = "~> 5.60" }
    random = { source = "hashicorp/random", version = "~> 3.6" }
  }
}

provider "aws" {
  region  = var.region
  profile = var.profile
  default_tags {
    tags = {
      Project      = "dam.rs"
      Env          = "dev"
      Owner        = "bassam@axelerant.com"
      ManagedBy    = "terraform"
      Teardownable = "true"
    }
  }
}

data "aws_caller_identity" "me" {}

data "aws_ami" "al2023_arm64" {
  most_recent = true
  owners      = ["amazon"]
  filter {
    name   = "name"
    values = ["al2023-ami-2023.*-arm64"]
  }
  filter {
    name   = "architecture"
    values = ["arm64"]
  }
  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# ---------- network ----------
resource "aws_vpc" "main" {
  cidr_block           = "10.20.0.0/16"
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags                 = { Name = "${var.name_prefix}-vpc" }
}

resource "aws_internet_gateway" "igw" {
  vpc_id = aws_vpc.main.id
  tags   = { Name = "${var.name_prefix}-igw" }
}

# Two public subnets in two AZs. The EC2 lives in subnet a; RDS needs a subnet
# group spanning >=2 AZs even for a single-AZ instance.
resource "aws_subnet" "public_a" {
  vpc_id                  = aws_vpc.main.id
  cidr_block              = "10.20.1.0/24"
  availability_zone       = "${var.region}a"
  map_public_ip_on_launch = true
  tags                    = { Name = "${var.name_prefix}-public-a" }
}

resource "aws_subnet" "public_b" {
  vpc_id                  = aws_vpc.main.id
  cidr_block              = "10.20.2.0/24"
  availability_zone       = "${var.region}b"
  map_public_ip_on_launch = true
  tags                    = { Name = "${var.name_prefix}-public-b" }
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.main.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.igw.id
  }
  tags = { Name = "${var.name_prefix}-public-rt" }
}

resource "aws_route_table_association" "a" {
  subnet_id      = aws_subnet.public_a.id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table_association" "b" {
  subnet_id      = aws_subnet.public_b.id
  route_table_id = aws_route_table.public.id
}

# ---------- security groups ----------
resource "aws_security_group" "app" {
  name        = "${var.name_prefix}-app-sg"
  description = "dam.rs app host: API in from operator IP only; egress all"
  vpc_id      = aws_vpc.main.id

  ingress {
    description = "damd API, operator only"
    from_port   = 8080
    to_port     = 8080
    protocol    = "tcp"
    cidr_blocks = [var.my_ip_cidr]
  }

  # Caddy: 80 is required for the Let's Encrypt ACME HTTP-01 challenge and the HTTPS redirect;
  # 443 serves HTTPS. Both are open to the internet so the cert can be issued and the domain reached.
  ingress {
    description = "HTTP (Caddy ACME + redirect)"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  ingress {
    description = "HTTPS (Caddy)"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  # No SSH ingress: host access is via SSM Session Manager (outbound 443 only).
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = { Name = "${var.name_prefix}-app-sg" }
}

resource "aws_security_group" "db" {
  name        = "${var.name_prefix}-db-sg"
  description = "Postgres, reachable only from the app host"
  vpc_id      = aws_vpc.main.id

  ingress {
    description     = "Postgres from app SG"
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [aws_security_group.app.id]
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = { Name = "${var.name_prefix}-db-sg" }
}

# ---------- KMS (SSE-KMS / BYOK deployment key) ----------
resource "aws_kms_key" "objects" {
  description             = "dam.rs dev object-store CMK (SSE-KMS)"
  deletion_window_in_days = 7
  enable_key_rotation     = true
  # Key policy grants the account root full control so IAM policies govern access
  # (breaks the role<->key policy cycle). The app role's own policy grants use.
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "AccountRoot"
      Effect    = "Allow"
      Principal = { AWS = "arn:aws:iam::${data.aws_caller_identity.me.account_id}:root" }
      Action    = "kms:*"
      Resource  = "*"
    }]
  })
}

resource "aws_kms_alias" "objects" {
  name          = "alias/${var.name_prefix}-objects"
  target_key_id = aws_kms_key.objects.key_id
}

# ---------- S3 object store ----------
resource "random_id" "bucket" {
  byte_length = 4
}

resource "aws_s3_bucket" "objects" {
  bucket        = "${var.name_prefix}-objects-${random_id.bucket.hex}"
  force_destroy = true # dev: allow teardown of a non-empty bucket
  tags          = { Name = "${var.name_prefix}-objects" }
}

resource "aws_s3_bucket_versioning" "objects" {
  bucket = aws_s3_bucket.objects.id
  versioning_configuration { status = "Enabled" }
}

resource "aws_s3_bucket_public_access_block" "objects" {
  bucket                  = aws_s3_bucket.objects.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "objects" {
  bucket = aws_s3_bucket.objects.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = "aws:kms"
      kms_master_key_id = aws_kms_key.objects.arn
    }
    bucket_key_enabled = true
  }
}

# ---------- S3 Inventory ----------
# A daily CSV report of every current object with its size and storage class, delivered back to this same
# bucket under `inventory/`. dam's integrity scrub reads the latest manifest once per run
# (dam_store::S3Store::with_inventory_prefix) instead of issuing one HEAD per placement — the manifest lands
# at inventory/<bucket>/daily/<date>/manifest.json, which is what DAMRS_STORAGE__INVENTORY_PREFIX points at.
#
# "Current" object versions only: the scrub reconciles the live object per placement, and object_placements
# carries no version id (that is the deferred Intelligent-Tiering decision, DECISIONS.md), so noncurrent
# versions would be rows the scrub can never match.
resource "aws_s3_bucket_inventory" "objects" {
  bucket                   = aws_s3_bucket.objects.id
  name                     = "daily"
  included_object_versions = "Current"

  schedule {
    frequency = "Daily"
  }

  optional_fields = ["Size", "StorageClass"]

  destination {
    bucket {
      format     = "CSV"
      bucket_arn = aws_s3_bucket.objects.arn
      prefix     = "inventory"
      # SSE-S3, not the bucket's CMK: an inventory report is object keys and sizes, not object content, and
      # SSE-S3 avoids granting the S3 service principal a key-policy grant on the CMK for the delivery to
      # succeed. The explicit header overrides the bucket's aws:kms default for these objects only.
      encryption {
        sse_s3 {}
      }
    }
  }

  # The service can only write once the bucket policy grants it; without this the first scheduled run can
  # race ahead of the policy.
  depends_on = [aws_s3_bucket_policy.objects]
}

# S3 Inventory writes as the S3 service principal, which needs an explicit bucket-policy grant even when the
# destination is the same bucket in the same account. The SourceAccount/SourceArn conditions scope it to this
# account's own inventory of this bucket; the block_public_policy setting above accepts it because a service
# principal with these conditions is not a public grant.
data "aws_iam_policy_document" "bucket" {
  statement {
    sid     = "AllowS3InventoryDelivery"
    effect  = "Allow"
    actions = ["s3:PutObject"]
    principals {
      type        = "Service"
      identifiers = ["s3.amazonaws.com"]
    }
    resources = ["${aws_s3_bucket.objects.arn}/inventory/*"]
    condition {
      test     = "ArnLike"
      variable = "aws:SourceArn"
      values   = [aws_s3_bucket.objects.arn]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [data.aws_caller_identity.me.account_id]
    }
    condition {
      test     = "StringEquals"
      variable = "s3:x-amz-acl"
      values   = ["bucket-owner-full-control"]
    }
  }
}

resource "aws_s3_bucket_policy" "objects" {
  bucket = aws_s3_bucket.objects.id
  policy = data.aws_iam_policy_document.bucket.json
}

# ---------- ECR ----------
resource "aws_ecr_repository" "damrs" {
  name                 = var.name_prefix
  image_tag_mutability = "MUTABLE"
  force_delete         = true
  image_scanning_configuration { scan_on_push = true }
}

# ---------- RDS PostgreSQL 17 (+pgvector via app migration) ----------
resource "random_password" "db" {
  length  = 24
  special = false # keep the URL free of shell/URL-escaping hazards
}

resource "aws_db_subnet_group" "main" {
  name       = "${var.name_prefix}-db-subnets"
  subnet_ids = [aws_subnet.public_a.id, aws_subnet.public_b.id]
  tags       = { Name = "${var.name_prefix}-db-subnets" }
}

resource "aws_db_instance" "main" {
  identifier                 = "${var.name_prefix}-pg"
  engine                     = "postgres"
  engine_version             = var.db_engine_version
  instance_class             = var.db_instance_class
  allocated_storage          = var.db_allocated_storage
  storage_type               = "gp3"
  storage_encrypted          = true # default aws/rds key; the CMK is for S3/BYOK validation
  db_name                    = "damrs"
  username                   = "damrs"
  password                   = random_password.db.result
  db_subnet_group_name       = aws_db_subnet_group.main.name
  vpc_security_group_ids     = [aws_security_group.db.id]
  publicly_accessible        = false
  multi_az                   = false
  skip_final_snapshot        = true
  deletion_protection        = false
  auto_minor_version_upgrade = true
  apply_immediately          = true
  tags                       = { Name = "${var.name_prefix}-pg" }
}

# ---------- SSM parameters (config/secrets the host reads at boot) ----------
resource "aws_ssm_parameter" "database_url" {
  name  = "/damrs/dev/database-url"
  type  = "SecureString"
  value = "postgres://damrs:${random_password.db.result}@${aws_db_instance.main.address}:5432/damrs"
}

# Production refuses the placeholder URL-signing key and AI sealing key (config validate()),
# so both are generated real and stored sealed for the host to read at boot.
resource "random_password" "url_signing_key" {
  length  = 48
  special = false
}

resource "random_password" "sealing_key" {
  length  = 48
  special = false
}

resource "aws_ssm_parameter" "url_signing_key" {
  name  = "/damrs/dev/url-signing-key"
  type  = "SecureString"
  value = random_password.url_signing_key.result
}

resource "aws_ssm_parameter" "sealing_key" {
  name  = "/damrs/dev/sealing-key"
  type  = "SecureString"
  value = random_password.sealing_key.result
}

# ---------- IAM: EC2 instance role ----------
resource "aws_iam_role" "app" {
  name = "${var.name_prefix}-app-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

# Session Manager access (no SSH / no key pair).
resource "aws_iam_role_policy_attachment" "ssm_core" {
  role       = aws_iam_role.app.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

data "aws_iam_policy_document" "app" {
  # Object CRUD + multipart + restore on the bucket's objects.
  statement {
    sid    = "Objects"
    effect = "Allow"
    actions = [
      "s3:GetObject", "s3:PutObject", "s3:DeleteObject",
      "s3:RestoreObject", "s3:GetObjectAttributes",
      "s3:AbortMultipartUpload", "s3:ListMultipartUploadParts"
    ]
    resources = ["${aws_s3_bucket.objects.arn}/*"]
  }
  # Bucket-level: ListBucket is required by /ready's store.list("readiness/",1)
  # (review finding 3.11) plus location/versions for the app.
  statement {
    sid       = "Bucket"
    effect    = "Allow"
    actions   = ["s3:ListBucket", "s3:GetBucketLocation", "s3:ListBucketMultipartUploads", "s3:ListBucketVersions"]
    resources = [aws_s3_bucket.objects.arn]
  }
  # KMS use for SSE-KMS (GenerateDataKey* on put, Decrypt on get).
  statement {
    sid       = "Kms"
    effect    = "Allow"
    actions   = ["kms:GenerateDataKey", "kms:GenerateDataKeyWithoutPlaintext", "kms:Decrypt", "kms:DescribeKey"]
    resources = [aws_kms_key.objects.arn]
  }
  # Read the app's own SSM parameters.
  statement {
    sid       = "SsmRead"
    effect    = "Allow"
    actions   = ["ssm:GetParameter", "ssm:GetParameters", "ssm:GetParametersByPath"]
    resources = ["arn:aws:ssm:${var.region}:${data.aws_caller_identity.me.account_id}:parameter/damrs/*"]
  }
  # Pull the image from ECR.
  statement {
    sid       = "EcrPull"
    effect    = "Allow"
    actions   = ["ecr:GetDownloadUrlForLayer", "ecr:BatchGetImage", "ecr:BatchCheckLayerAvailability"]
    resources = [aws_ecr_repository.damrs.arn]
  }
  statement {
    sid       = "EcrAuth"
    effect    = "Allow"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"]
  }
}

resource "aws_iam_role_policy" "app" {
  name   = "${var.name_prefix}-app-policy"
  role   = aws_iam_role.app.id
  policy = data.aws_iam_policy_document.app.json
}

resource "aws_iam_instance_profile" "app" {
  name = "${var.name_prefix}-app-profile"
  role = aws_iam_role.app.name
}

# ---------- EC2 app host ----------
resource "aws_instance" "app" {
  ami                    = data.aws_ami.al2023_arm64.id
  instance_type          = var.instance_type
  subnet_id              = aws_subnet.public_a.id
  vpc_security_group_ids = [aws_security_group.app.id]
  iam_instance_profile   = aws_iam_instance_profile.app.name

  # IMDSv2 required + hop limit 1: compensating control for the SSRF findings
  # (review 3.7/3.8/3.9) so a proxied request can't reach instance credentials.
  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_size = 20
    volume_type = "gp3"
    encrypted   = true
  }

  user_data = templatefile("${path.module}/user_data.sh.tftpl", {
    region            = var.region
    ecr_repo_url      = aws_ecr_repository.damrs.repository_url
    image_tag         = var.image_tag
    bucket            = aws_s3_bucket.objects.bucket
    inventory_prefix  = "inventory/${aws_s3_bucket.objects.bucket}/${aws_s3_bucket_inventory.objects.name}"
    kms_key_arn       = aws_kms_key.objects.arn
    db_url_param      = aws_ssm_parameter.database_url.name
    signing_key_param = aws_ssm_parameter.url_signing_key.name
    sealing_key_param = aws_ssm_parameter.sealing_key.name
  })

  tags = { Name = "${var.name_prefix}-app" }
}

# ---------- Elastic IP + DNS (stable address for Caddy/HTTPS) ----------
resource "aws_eip" "app" {
  domain   = "vpc"
  instance = aws_instance.app.id
  tags     = { Name = "${var.name_prefix}-eip" }
}

data "aws_route53_zone" "axelerant" {
  name         = "axelerant.tech."
  private_zone = false
}

resource "aws_route53_record" "damrs" {
  zone_id = data.aws_route53_zone.axelerant.zone_id
  name    = "damrs.axelerant.tech"
  type    = "A"
  ttl     = 300
  records = [aws_eip.app.public_ip]
}
