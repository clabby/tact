# Binary carrier image. Buildx Bake resolves relative paths against the
# caller's working directory, so invoke this file from the repository root.

variable "REGISTRY" {
  default = "ghcr.io"
}

variable "REPOSITORY" {
  default = "clabby/tact"
}

variable "DEFAULT_TAG" {
  default = "local"
}

group "default" {
  targets = ["tact"]
}

target "tact" {
  context = "."
  dockerfile = "docker/tact.dockerfile"
  tags = [
    "tact:${DEFAULT_TAG}",
    "${REGISTRY}/${REPOSITORY}:${DEFAULT_TAG}",
  ]
}
