terraform {
  required_version = ">= 1.5"
}

provider "aws" {
  region = var.region
}

locals {
  bucket_name = "${var.prefix}-logs"
}

resource "aws_s3_bucket" "logs" {
  bucket = local.bucket_name
}

data "aws_iam_policy_document" "read" {
  statement {
    resources = ["${aws_s3_bucket.logs.arn}/*"]
  }
}

module "vpc" {
  source = "./modules/vpc"
  cidr   = var.cidr
}
