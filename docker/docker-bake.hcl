variable "RELEASE_BINARY_CONTEXT" {
  default = "."
}

variable "RELEASE_REVISION" {
  default = "local"
}

variable "RELEASE_VERSION" {
  default = "local"
}

group "default" {
  targets = ["dev"]
}

target "dev" {
  context = "."
  dockerfile = "docker/development.dockerfile"
  tags = ["tact-dev:local"]
}

target "release" {
  context = "docker"
  contexts = {
    binary = "${RELEASE_BINARY_CONTEXT}"
  }
  dockerfile = "release.dockerfile"
  labels = {
    "org.opencontainers.image.revision" = "${RELEASE_REVISION}"
    "org.opencontainers.image.version" = "${RELEASE_VERSION}"
  }
}
