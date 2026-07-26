# This target is intentionally local-only and is not part of the repository's
# release Bake file or its default target group.
target "default" {
  # Bake resolves this against the caller's directory. The supported recipes
  # invoke it from the repository root.
  context = "."
  dockerfile = "docker/dev/Dockerfile"
  tags = ["tact-dev:local"]
}
