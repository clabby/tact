#!/usr/bin/env python3
"""Launch the repository in its isolated local development containers."""

from __future__ import annotations

import argparse
import json
import os
import signal
import stat
import subprocess
import sys
import tempfile
import tomllib
import uuid
from collections.abc import Mapping, Sequence
from pathlib import Path

import config


DEVELOPMENT_DIRECTORY = Path(__file__).resolve().parent
TOOLS_FILE = DEVELOPMENT_DIRECTORY / "tools.toml"
DOCKER_TIMEOUT_SECONDS = 120
CLEANUP_TIMEOUT_SECONDS = 30
PERSISTENT_VOLUME = "tact-dev-state"

DOCKER_ENVIRONMENT_NAMES = (
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
    "DOCKER_HOST",
    "PATH",
    "XDG_RUNTIME_DIR",
)


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser(
        description="Run Tact in the local development container.",
        epilog="Place `--` before arguments intended for Tact.",
    )
    argument_parser.add_argument(
        "--workspace",
        type=Path,
        default=Path.cwd(),
        help="workspace mounted read-write at /workspace (default: current directory)",
    )
    argument_parser.add_argument(
        "--auth",
        choices=("auto", "api-key", "chatgpt"),
        default="auto",
    )
    argument_parser.add_argument(
        "--no-instructions",
        action="store_true",
        help="do not project global AGENTS instructions",
    )
    argument_parser.add_argument(
        "--no-skills", action="store_true", help="do not project local skill roots"
    )
    argument_parser.add_argument(
        "--skill-root",
        action="append",
        default=[],
        type=Path,
        metavar="PATH",
        help="additional skill root to project; may be repeated",
    )
    operation = argument_parser.add_mutually_exclusive_group()
    operation.add_argument(
        "--shell", action="store_true", help="run an interactive Bash shell instead of Tact"
    )
    operation.add_argument(
        "--clean",
        action="store_true",
        help=f"remove the persistent {PERSISTENT_VOLUME} volume and exit",
    )
    argument_parser.add_argument("tact_arguments", nargs=argparse.REMAINDER)
    return argument_parser


def run(
    arguments: argparse.Namespace,
    environment: Mapping[str, str] = os.environ,
) -> int:
    # This validation intentionally precedes every credential read.
    require_local_linux_docker(environment)
    if arguments.clean:
        return _clean_persistent_volume(environment)
    _ensure_persistent_volume(environment)

    workspace = arguments.workspace.expanduser().resolve(strict=True)
    if not workspace.is_dir():
        raise RuntimeError(f"workspace is not a directory: {workspace}")
    if not os.access(workspace, os.W_OK):
        raise RuntimeError(f"workspace is not writable: {workspace}")

    state_directory, state_config_name = prepare_tact_state(environment)

    compose_file = DEVELOPMENT_DIRECTORY / "compose.yaml"
    project_name = f"tact-dev-{uuid.uuid4().hex}"

    with tempfile.TemporaryDirectory(prefix="tact-development-") as temporary:
        temporary_directory = Path(temporary)
        temporary_directory.chmod(0o700)
        private_directory = temporary_directory / "private"
        public_directory = temporary_directory / "public"
        private_directory.mkdir(mode=0o700)
        public_directory.mkdir(mode=0o700)

        iron_proxy_image = _iron_proxy_image()
        generate_proxy_ca(private_directory, environment, iron_proxy_image)
        config.write_public_file(
            public_directory / "ca.crt", (private_directory / "ca.crt").read_bytes()
        )

        credentials = config.select_credentials(arguments.auth, environment)
        with credentials:
            ensure_credentials_not_mounted(
                credentials, (workspace, state_directory)
            )
            config.write_auth_projection(
                credentials, private_directory, public_directory
            )

        config.project_local_agent_files(
            public_directory,
            environment,
            include_instructions=not arguments.no_instructions,
            include_skills=not arguments.no_skills,
            additional_skill_roots=arguments.skill_root,
        )
        tact_arguments = list(arguments.tact_arguments)
        if tact_arguments and tact_arguments[0] == "--":
            tact_arguments.pop(0)
        override_file = _write_compose_override(
            public_directory,
            tact_arguments,
            shell=arguments.shell,
        )
        compose_environment = _compose_environment(
            environment,
            workspace=workspace,
            state_directory=state_directory,
            state_config_name=state_config_name,
            private_directory=private_directory,
            public_directory=public_directory,
            auth_mode=credentials.mode,
            iron_proxy_image=iron_proxy_image,
        )
        compose_arguments = [
            "docker",
            "compose",
            "--file",
            str(compose_file),
            "--file",
            str(override_file),
            "--project-name",
            project_name,
        ]

        active_error: BaseException | None = None
        try:
            return _run_compose(compose_arguments, compose_environment)
        except BaseException as error:
            active_error = error
            raise
        finally:
            try:
                subprocess.run(
                    [*compose_arguments, "down", "--remove-orphans"],
                    env=compose_environment,
                    check=True,
                    timeout=CLEANUP_TIMEOUT_SECONDS,
                )
            except Exception as cleanup_error:
                if active_error is None:
                    raise RuntimeError("failed to clean up development containers") from cleanup_error
                print(
                    f"warning: failed to clean up development containers: {cleanup_error}",
                    file=sys.stderr,
                )


def require_local_linux_docker(
    environment: Mapping[str, str] = os.environ,
) -> None:
    endpoint = environment.get("DOCKER_HOST", "").strip()
    docker_environment = _docker_environment(environment)
    if not endpoint:
        result = _run_docker(
            ["context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
            docker_environment,
            timeout=10,
        )
        endpoint = result.stdout.strip()
    if not endpoint.startswith(("unix://", "/")):
        raise RuntimeError(
            f"development authentication requires a local Docker socket, got {endpoint!r}"
        )

    result = _run_docker(
        ["info", "--format", "{{.OSType}}"], docker_environment, timeout=15
    )
    if result.stdout.strip() != "linux":
        raise RuntimeError("the development container requires Linux Docker containers")


def prepare_tact_state(
    environment: Mapping[str, str] = os.environ,
) -> tuple[Path, str]:
    selected_config = config.default_tact_config_file(environment)
    if not selected_config.name:
        raise RuntimeError(f"Tact config path has no filename: {selected_config}")

    state_directory = selected_config.parent
    state_directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    state_directory = state_directory.resolve(strict=True)
    if not os.access(state_directory, os.W_OK):
        raise RuntimeError(f"Tact state directory is not writable: {state_directory}")

    mounted_config = state_directory / selected_config.name
    try:
        metadata = mounted_config.lstat()
    except FileNotFoundError:
        descriptor = os.open(
            mounted_config, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
        )
        os.close(descriptor)
        return state_directory, selected_config.name

    if stat.S_ISLNK(metadata.st_mode):
        raise RuntimeError(f"Tact config must not be a symlink: {selected_config}")
    if not stat.S_ISREG(metadata.st_mode):
        raise RuntimeError(f"Tact config is not a regular file: {selected_config}")
    if not os.access(mounted_config, os.R_OK):
        raise RuntimeError(f"Tact config is not readable: {selected_config}")
    return state_directory, selected_config.name


def ensure_credentials_not_mounted(
    credentials: config.Credentials, writable_roots: Sequence[Path]
) -> None:
    source = credentials.source_path
    if source is None:
        return
    if not any(source.is_relative_to(root) for root in writable_roots):
        return
    raise RuntimeError(
        f"credential file is inside a writable container mount: {source}"
    )


def generate_proxy_ca(
    private_directory: Path,
    environment: Mapping[str, str],
    image: str,
) -> None:
    _run_docker(
        [
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            f"{os.getuid()}:{os.getgid()}",
            "--volume",
            f"{private_directory}:/out",
            image,
            "generate-ca",
            "-outdir",
            "/out",
            "-alg",
            "ed25519",
            "-name",
            "Tact local development",
        ],
        _docker_environment(environment),
        timeout=DOCKER_TIMEOUT_SECONDS,
    )
    for name in ("ca.crt", "ca.key"):
        path = private_directory / name
        metadata = path.lstat()
        if path.is_symlink() or not stat.S_ISREG(metadata.st_mode):
            raise RuntimeError(f"iron-proxy did not generate a regular {name}")
        path.chmod(0o600)


def _write_compose_override(
    public_directory: Path, tact_arguments: Sequence[str], *, shell: bool = False
) -> Path:
    path = public_directory / "compose.override.json"
    service: dict[str, object] = {"command": list(tact_arguments)}
    if shell:
        if tact_arguments:
            raise RuntimeError("--shell does not accept Tact arguments")
        service = {"command": [], "environment": {"TACT_DEV_SHELL": "1"}}
    document = {"services": {"tact-dev": service}}
    config.write_public_file(
        path, json.dumps(document, separators=(",", ":")).encode()
    )
    return path


def _compose_environment(
    source: Mapping[str, str],
    *,
    workspace: Path,
    state_directory: Path,
    state_config_name: str,
    private_directory: Path,
    public_directory: Path,
    auth_mode: str,
    iron_proxy_image: str,
) -> dict[str, str]:
    result = _docker_environment(source)
    result.update(
        {
            # This is the complete host-to-Compose interface. Real credentials are
            # deliberately absent; Compose receives only paths and safe selectors.
            "TACT_DEV_WORKSPACE": str(workspace),
            "TACT_DEV_STATE_DIR": str(state_directory),
            "TACT_DEV_CONFIG_NAME": state_config_name,
            "TACT_DEV_PRIVATE_DIR": str(private_directory),
            "TACT_DEV_PUBLIC_DIR": str(public_directory),
            "TACT_DEV_UID": str(os.getuid()),
            "TACT_DEV_GID": str(os.getgid()),
            "TACT_DEV_AUTH": auth_mode,
            "TACT_DEV_IRON_IMAGE": iron_proxy_image,
            "TACT_DEV_IMAGE": source.get("TACT_DEV_IMAGE", "tact-dev:local"),
        }
    )
    return result


def _iron_proxy_image() -> str:
    try:
        with TOOLS_FILE.open("rb") as manifest_file:
            manifest = tomllib.load(manifest_file)
        image = manifest["images"]["iron_proxy"]
    except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as error:
        raise RuntimeError(f"failed to read iron-proxy image from {TOOLS_FILE}") from error
    if not isinstance(image, str) or "@sha256:" not in image:
        raise RuntimeError("iron-proxy image must be pinned by digest")
    return image


def _docker_environment(source: Mapping[str, str]) -> dict[str, str]:
    return {name: source[name] for name in DOCKER_ENVIRONMENT_NAMES if source.get(name)}


def _clean_persistent_volume(environment: Mapping[str, str]) -> int:
    docker_environment = _docker_environment(environment)
    try:
        result = subprocess.run(
            ["docker", "volume", "rm", PERSISTENT_VOLUME],
            env=docker_environment,
            capture_output=True,
            text=True,
            check=False,
            timeout=CLEANUP_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("failed to remove the development volume") from error
    if result.returncode == 0 or "no such volume" in result.stderr.lower():
        return 0
    detail = (result.stderr or result.stdout or "unknown Docker error").strip()
    raise RuntimeError(f"failed to remove {PERSISTENT_VOLUME}: {detail}")


def _ensure_persistent_volume(environment: Mapping[str, str]) -> None:
    _run_docker(
        ["volume", "create", PERSISTENT_VOLUME],
        _docker_environment(environment),
        timeout=CLEANUP_TIMEOUT_SECONDS,
    )


def _run_docker(
    arguments: Sequence[str],
    environment: Mapping[str, str],
    *,
    timeout: int,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            ["docker", *arguments],
            env=environment,
            capture_output=True,
            text=True,
            check=False,
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("failed to run local Docker") from error
    if result.returncode != 0:
        detail = (result.stderr or result.stdout or "unknown Docker error").strip()
        raise RuntimeError(f"local Docker command failed: {detail}")
    return result


def _run_compose(
    compose_arguments: Sequence[str], environment: Mapping[str, str]
) -> int:
    command = [
        *compose_arguments,
        "run",
        "--rm",
        "tact-dev",
    ]
    try:
        process = subprocess.Popen(command, env=environment)
    except OSError as error:
        raise RuntimeError("failed to start Docker Compose") from error

    previous_handlers: dict[int, signal.Handlers] = {}

    def forward(signum: int, _frame: object) -> None:
        if process.poll() is None:
            process.send_signal(signum)

    for signum in (signal.SIGINT, signal.SIGTERM):
        previous_handlers[signum] = signal.signal(signum, forward)
    try:
        return process.wait()
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)


def main(argv: Sequence[str] | None = None) -> int:
    try:
        return run(parser().parse_args(argv))
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
