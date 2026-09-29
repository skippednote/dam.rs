output "api_url" {
  value = "http://${aws_instance.app.public_ip}:8080"
}

output "ec2_public_ip" {
  value = aws_instance.app.public_ip
}

output "ec2_instance_id" {
  description = "For SSM Session Manager: aws ssm start-session --target <id>"
  value       = aws_instance.app.id
}

output "ecr_repo_url" {
  value = aws_ecr_repository.damrs.repository_url
}

output "s3_bucket" {
  value = aws_s3_bucket.objects.bucket
}

output "kms_key_arn" {
  value = aws_kms_key.objects.arn
}

output "rds_endpoint" {
  value = aws_db_instance.main.address
}

output "region" {
  value = var.region
}

output "domain" {
  value = "https://damrs.axelerant.tech"
}

output "elastic_ip" {
  value = aws_eip.app.public_ip
}
