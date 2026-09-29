variable "region" {
  type    = string
  default = "ap-south-1"
}

variable "profile" {
  type    = string
  default = "axelerant"
}

# Short, DNS/name-safe prefix for every resource. The pretty "dam.rs" lives in tags, not names.
variable "name_prefix" {
  type    = string
  default = "damrs-dev"
}

# Inbound API/SSH access is locked to just this address. No default on purpose: it is an
# operator's own IP, so it is set in terraform.tfvars (gitignored) rather than committed.
# See terraform.tfvars.example.
variable "my_ip_cidr" {
  type = string
}

variable "instance_type" {
  type    = string
  default = "t4g.medium" # arm64/Graviton. medium (4 GB): small's 2 GB swap-locks under bulk ingest + AI enrichment.
}

variable "db_instance_class" {
  type    = string
  default = "db.t3.micro"
}

variable "db_allocated_storage" {
  type    = number
  default = 20
}

variable "db_engine_version" {
  type    = string
  default = "17.6" # ap-south-1 offers 17.5-17.11; auto_minor_version_upgrade handles patches
}

variable "image_tag" {
  type    = string
  default = "latest"
}
