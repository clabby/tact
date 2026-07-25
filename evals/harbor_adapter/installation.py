"""Build task-side installation commands for the Tact Harbor adapter."""


def cli_tools_install_command(*, install_node: bool) -> str:
    """Build a portable installer for Tact's task-side CLI dependencies."""
    packages = ["ca-certificates", "curl", "bash", "ripgrep"]
    checks = ["curl", "bash", "rg"]
    if install_node:
        packages.extend(("nodejs", "npm"))
        checks.extend(("node", "npm"))

    package_list = " ".join(packages)
    command_checks = "; ".join(
        f"command -v {command} >/dev/null 2>&1" for command in checks
    )
    return (
        "if ldd --version 2>&1 | grep -qi musl || "
        "[ -f /etc/alpine-release ]; then "
        f"apk add --no-cache {package_list}; "
        "elif command -v apt-get >/dev/null 2>&1; then "
        "apt-get update && DEBIAN_FRONTEND=noninteractive "
        "apt-get install --yes --no-install-recommends "
        f"{package_list}; "
        "elif command -v yum >/dev/null 2>&1; then "
        f"yum install -y {package_list}; "
        "else "
        "echo 'No supported package manager found; checking preinstalled tools' >&2; "
        "fi; "
        f"{command_checks}"
    )
