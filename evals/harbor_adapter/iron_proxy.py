"""Run a local iron-proxy beside one untrusted Harbor task container."""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import tempfile
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Sequence
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any

import yaml
from harbor.environments.base import BaseEnvironment
from harbor.environments.docker.docker import DockerEnvironment

from harbor_adapter.credentials import (
    DEFAULT_MINIMUM_LIFETIME_SECONDS,
    FAKE_ACCESS_TOKEN,
    FAKE_ACCOUNT_ID,
    fake_auth_document,
    read_codex_credentials,
)


IRON_PROXY_IMAGE = (
    "ironsh/iron-proxy:0.49.0@"
    "sha256:c4628019c24f4cc8d77564a26b7c9cedb00accee6f93d06270e85fb8f9c6a7da"
)
REMOTE_AUTH_DIRECTORY = "/tmp/tact-harbor-auth"
REMOTE_AUTH_FILE = f"{REMOTE_AUTH_DIRECTORY}/auth.json"
REMOTE_CA_FILE = f"{REMOTE_AUTH_DIRECTORY}/ca.crt"
PROXY_URL = "http://127.0.0.1:8080"
CLEANUP_TIMEOUT_SECONDS = 30
CODEX_METHODS = ["GET", "POST"]
CODEX_PATHS = ["/backend-api/codex", "/backend-api/codex/*"]

EnvironmentCommand = Callable[..., Awaitable[Any]]


class LocalCodexAuthProxy:
    """Give iron the real access token while the task sees only mock auth."""

    def __init__(
        self,
        path: str | Path,
        minimum_lifetime_seconds: int = DEFAULT_MINIMUM_LIFETIME_SECONDS,
        safe_bind_root: str | Path | None = None,
    ) -> None:
        self.path = Path(path).expanduser().absolute()
        self.minimum_lifetime_seconds = minimum_lifetime_seconds
        self.safe_bind_root = (
            Path(safe_bind_root).resolve() if safe_bind_root is not None else None
        )

    @property
    def agent_environment(self) -> dict[str, str]:
        return {
            "HTTP_PROXY": PROXY_URL,
            "HTTPS_PROXY": PROXY_URL,
            "http_proxy": PROXY_URL,
            "https_proxy": PROXY_URL,
            "NO_PROXY": "",
            "no_proxy": "",
            "SSL_CERT_FILE": REMOTE_CA_FILE,
            "REQUESTS_CA_BUNDLE": REMOTE_CA_FILE,
            "CURL_CA_BUNDLE": REMOTE_CA_FILE,
            "GIT_SSL_CAINFO": REMOTE_CA_FILE,
            "NODE_EXTRA_CA_CERTS": REMOTE_CA_FILE,
        }

    @staticmethod
    def require_local_docker(environment: BaseEnvironment) -> DockerEnvironment:
        if type(environment) is not DockerEnvironment:
            raise RuntimeError(
                "ChatGPT authentication requires Harbor's local Docker environment"
            )
        if environment._is_windows_container:
            raise RuntimeError("ChatGPT authentication requires a Linux task container")

        endpoint = _docker_endpoint()
        if endpoint.startswith(("unix://", "/")):
            return environment
        raise RuntimeError(
            f"ChatGPT authentication requires a local Docker socket, got {endpoint!r}"
        )

    @asynccontextmanager
    async def running(
        self,
        environment: BaseEnvironment,
        *,
        exec_as_agent: EnvironmentCommand,
        exec_as_root: EnvironmentCommand,
    ) -> AsyncIterator[None]:
        docker_environment = self.require_local_docker(environment)
        container_name = f"tact-iron-{uuid.uuid4().hex}"

        with tempfile.TemporaryDirectory(prefix="tact-iron-proxy-") as directory:
            proxy_directory = Path(directory)
            await self._generate_ca(proxy_directory)
            self._write_public_files(proxy_directory)
            active_error: BaseException | None = None
            try:
                await self._stage_public_files(
                    environment,
                    proxy_directory,
                    exec_as_root=exec_as_root,
                )
                task_container = await self._main_container_id(docker_environment)
                task_containers = await self._task_container_ids(docker_environment)
                if task_container not in task_containers:
                    raise RuntimeError("Harbor's main task container is not running")
                for container_id in task_containers:
                    await self._require_isolated_task(container_id)
                self._write_proxy_credentials(proxy_directory)
                await self._start_proxy(
                    proxy_directory,
                    task_container=task_container,
                    container_name=container_name,
                )
                await exec_as_agent(
                    environment,
                    "for attempt in $(seq 1 100); do "
                    "bash -c 'exec 3<>/dev/tcp/127.0.0.1/8080' 2>/dev/null && exit 0; "
                    "sleep 0.1; done; exit 1",
                    timeout_sec=15,
                )
                yield
            except BaseException as error:
                active_error = error
                raise
            finally:
                cleanup_error = await self._finish_cleanup(
                    environment,
                    container_name=container_name,
                    exec_as_root=exec_as_root,
                )
                if cleanup_error is not None:
                    if active_error is not None:
                        raise cleanup_error from active_error
                    raise cleanup_error

    def _write_proxy_credentials(self, directory: Path) -> None:
        credentials = read_codex_credentials(
            self.path,
            minimum_lifetime_seconds=self.minimum_lifetime_seconds,
        )
        with credentials:
            _write_private_file(directory / "access-token", credentials.access_token)
            _write_private_file(directory / "account-id", credentials.account_id)

    async def _generate_ca(self, directory: Path) -> None:
        await _run_docker(
            [
                "run",
                "--rm",
                "--network",
                "none",
                "--user",
                _host_user(),
                "--volume",
                f"{directory}:/out",
                IRON_PROXY_IMAGE,
                "generate-ca",
                "-outdir",
                "/out",
                "-alg",
                "ed25519",
                "-name",
                "Tact Harbor local evaluation",
            ],
            timeout_seconds=120,
        )

    def _write_public_files(self, directory: Path) -> None:
        config = {
            "dns": {"enabled": False},
            "proxy": {"tunnel_listen": "127.0.0.1:8080"},
            "tls": {
                "ca_cert": "/run/tact-auth/ca.crt",
                "ca_key": "/run/tact-auth/ca.key",
            },
            "transforms": [
                {
                    "name": "allowlist",
                    "config": {
                        "rules": [
                            {
                                "host": "chatgpt.com",
                                "methods": ["CONNECT"],
                            },
                            {
                                "host": "chatgpt.com",
                                "methods": CODEX_METHODS,
                                "paths": CODEX_PATHS,
                            }
                        ]
                    },
                },
                {
                    "name": "secrets",
                    "config": {
                        "secrets": [
                            _secret_swap(
                                source="access-token",
                                proxy_value=FAKE_ACCESS_TOKEN,
                                header="Authorization",
                            ),
                            _secret_swap(
                                source="account-id",
                                proxy_value=FAKE_ACCOUNT_ID,
                                header="chatgpt-account-id",
                            ),
                        ]
                    },
                },
            ],
            "log": {"level": "warn"},
        }
        (directory / "proxy.yaml").write_text(
            yaml.safe_dump(config, sort_keys=False), encoding="utf-8"
        )
        (directory / "fake-auth.json").write_bytes(fake_auth_document())

    async def _stage_public_files(
        self,
        environment: BaseEnvironment,
        directory: Path,
        *,
        exec_as_root: EnvironmentCommand,
    ) -> None:
        await exec_as_root(
            environment,
            f"rm -rf -- {REMOTE_AUTH_DIRECTORY} && "
            f"mkdir {REMOTE_AUTH_DIRECTORY} && chmod 0755 {REMOTE_AUTH_DIRECTORY}",
        )
        await environment.upload_file(directory / "fake-auth.json", REMOTE_AUTH_FILE)
        await environment.upload_file(directory / "ca.crt", REMOTE_CA_FILE)
        await exec_as_root(
            environment,
            f"chmod 0444 {REMOTE_AUTH_FILE} {REMOTE_CA_FILE}",
        )

    @staticmethod
    async def _main_container_id(environment: DockerEnvironment) -> str:
        result = await environment._run_docker_compose_command(["ps", "-q", "main"])
        container_ids = (result.stdout or "").split()
        if len(container_ids) != 1:
            raise RuntimeError("failed to identify Harbor's main task container")
        return container_ids[0]

    @staticmethod
    async def _task_container_ids(environment: DockerEnvironment) -> list[str]:
        result = await environment._run_docker_compose_command(["ps", "-q"])
        container_ids = (result.stdout or "").split()
        if not container_ids:
            raise RuntimeError("failed to identify Harbor's task containers")
        return container_ids

    async def _require_isolated_task(self, container_id: str) -> None:
        result = await _run_docker(
            ["inspect", container_id],
            timeout_seconds=30,
        )
        try:
            documents = json.loads(result.stdout)
            document = documents[0]
            host = document["HostConfig"]
            mounts = document["Mounts"]
        except (IndexError, KeyError, TypeError, json.JSONDecodeError) as error:
            raise RuntimeError("failed to inspect Harbor's task isolation") from error

        unsafe_modes = {
            "privileged": bool(host.get("Privileged")),
            "host PID": host.get("PidMode") == "host",
            "host network": host.get("NetworkMode") == "host",
            "host IPC": host.get("IpcMode") == "host",
            "added capabilities": bool(host.get("CapAdd")),
            "host devices": bool(host.get("Devices")),
        }
        enabled = [name for name, present in unsafe_modes.items() if present]
        if enabled:
            raise RuntimeError(
                "refusing to expose subscription auth near an unsafe task container: "
                + ", ".join(enabled)
            )

        allowed_destinations = {"/logs/agent", "/logs/verifier", "/logs/artifacts"}
        unsafe_binds = []
        for mount in mounts:
            if mount.get("Type") != "bind":
                continue
            source = _host_mount_path(mount.get("Source", ""))
            destination = mount.get("Destination", "")
            safe_source = self.safe_bind_root is not None and source.is_relative_to(
                self.safe_bind_root
            )
            if destination not in allowed_destinations or not safe_source:
                unsafe_binds.append(destination)
        if unsafe_binds:
            raise RuntimeError(
                "refusing to expose subscription auth near task bind mounts: "
                + ", ".join(unsafe_binds)
            )

    async def _start_proxy(
        self,
        directory: Path,
        *,
        task_container: str,
        container_name: str,
    ) -> None:
        await _run_docker(
            [
                "run",
                "--detach",
                "--name",
                container_name,
                "--network",
                f"container:{task_container}",
                "--user",
                _host_user(),
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--volume",
                f"{directory}:/run/tact-auth:ro",
                IRON_PROXY_IMAGE,
                "--config",
                "/run/tact-auth/proxy.yaml",
            ],
            timeout_seconds=120,
        )

    async def _finish_cleanup(
        self,
        environment: BaseEnvironment,
        *,
        container_name: str,
        exec_as_root: EnvironmentCommand,
    ) -> Exception | None:
        cleanup = asyncio.create_task(
            self._cleanup(
                environment,
                container_name=container_name,
                exec_as_root=exec_as_root,
            )
        )
        try:
            await asyncio.shield(cleanup)
        except asyncio.CancelledError:
            while not cleanup.done():
                try:
                    await asyncio.shield(cleanup)
                except asyncio.CancelledError:
                    continue
            raise
        except Exception as error:
            return error
        return None

    async def _cleanup(
        self,
        environment: BaseEnvironment,
        *,
        container_name: str,
        exec_as_root: EnvironmentCommand,
    ) -> None:
        results = await asyncio.gather(
            _run_docker(
                ["rm", "--force", container_name],
                timeout_seconds=CLEANUP_TIMEOUT_SECONDS,
            ),
            exec_as_root(
                environment,
                f"rm -rf -- {REMOTE_AUTH_DIRECTORY}",
                timeout_sec=CLEANUP_TIMEOUT_SECONDS,
            ),
            return_exceptions=True,
        )
        errors = [result for result in results if isinstance(result, BaseException)]
        if len(errors) == 1:
            raise errors[0]
        if errors:
            raise BaseExceptionGroup("iron-proxy cleanup failed", errors)


def _secret_swap(*, source: str, proxy_value: str, header: str) -> dict[str, object]:
    return {
        "source": {"type": "file", "path": f"/run/tact-auth/{source}"},
        "replace": {
            "proxy_value": proxy_value,
            "match_headers": [header],
            "require": True,
        },
        "rules": [
            {
                "host": "chatgpt.com",
                "methods": CODEX_METHODS,
                "paths": CODEX_PATHS,
            }
        ],
    }


def _write_private_file(path: Path, value: bytearray) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(descriptor, "wb") as file:
        file.write(value)


def _host_user() -> str:
    return f"{os.getuid()}:{os.getgid()}"


def _host_mount_path(source: str) -> Path:
    for prefix in ("/host_mnt", "/run/desktop/mnt/host"):
        if source.startswith(f"{prefix}/"):
            source = source[len(prefix) :]
            break
    return Path(source).resolve()


async def _run_docker(
    arguments: Sequence[str],
    *,
    check: bool = True,
    timeout_seconds: int,
) -> subprocess.CompletedProcess[str]:
    try:
        process = await asyncio.create_subprocess_exec(
            "docker",
            *arguments,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
    except OSError as error:
        raise RuntimeError("failed to run the local iron-proxy container") from error
    communicate = asyncio.create_task(process.communicate())
    deadline = asyncio.get_running_loop().time() + timeout_seconds
    try:
        stdout, stderr = await asyncio.wait_for(
            asyncio.shield(communicate), timeout=timeout_seconds
        )
    except asyncio.CancelledError as cancellation:
        while not communicate.done():
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                process.kill()
                await asyncio.shield(communicate)
                break
            try:
                await asyncio.wait_for(asyncio.shield(communicate), timeout=remaining)
            except asyncio.CancelledError:
                continue
            except TimeoutError:
                process.kill()
                await asyncio.shield(communicate)
                break
        raise cancellation
    except TimeoutError as error:
        process.kill()
        await communicate
        raise RuntimeError("local iron-proxy Docker command timed out") from error

    result = subprocess.CompletedProcess(
        ["docker", *arguments],
        process.returncode,
        stdout.decode(errors="replace"),
        stderr.decode(errors="replace"),
    )
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout or "unknown Docker error").strip()
        raise RuntimeError(f"failed to run the local iron-proxy container: {detail}")
    return result


def _docker_endpoint() -> str:
    configured_endpoint = os.environ.get("DOCKER_HOST", "").strip()
    if configured_endpoint:
        return configured_endpoint
    try:
        result = subprocess.run(
            ["docker", "context", "inspect", "--format", "{{.Endpoints.docker.Host}}"],
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        raise RuntimeError("failed to verify that Docker is local") from error
    endpoint = result.stdout.strip()
    if result.returncode != 0 or not endpoint:
        raise RuntimeError("failed to verify that Docker is local")
    return endpoint
