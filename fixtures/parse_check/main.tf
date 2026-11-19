# Valid Terraform that an edit may write: for expressions, templates and heredocs.
terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "names" {
  type = list(object({
    name = string
    size = optional(number, 1)
  }))
  default = []
}

locals {
  upper   = [for item in var.names : upper(item.name) if item.size > 0]
  by_name = { for item in var.names : item.name => item.size... }
  label   = "%{if length(var.names) > 0}some%{else}none%{endif}"
}

resource "aws_instance" "web" {
  count         = length(var.names)
  ami           = data.aws_ami.ubuntu.id
  instance_type = count.index == 0 ? "t3.small" : "t3.micro"
  tags = {
    Name = "web-${count.index}"
  }

  dynamic "ebs_block_device" {
    for_each = var.names
    content {
      device_name = "/dev/sd${ebs_block_device.key}"
    }
  }

  user_data = <<-EOT
    #!/bin/bash
    echo "${local.label}"
  EOT
}

output "ids" {
  value = aws_instance.web[*].id
}

moved {
  from = aws_instance.old
  to   = aws_instance.web
}
