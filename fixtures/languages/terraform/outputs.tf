output "bucket_arn" {
  value = aws_s3_bucket.logs.arn
}

output "read_policy" {
  value      = data.aws_iam_policy_document.read.json
  depends_on = [module.vpc]
}
