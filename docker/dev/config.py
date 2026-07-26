"""Prepare proxy credentials and non-secret local agent files."""

from __future__ import annotations

import base64
import json
import os
import re
import shutil
import stat
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping


MAX_AUTH_FILE_BYTES = 1024 * 1024
MAX_PROJECTED_FILE_BYTES = 16 * 1024 * 1024
DEFAULT_MINIMUM_LIFETIME_SECONDS = 60 * 60

PROXY_AUTH_DIRECTORY = Path("/run/tact-auth")

FAKE_API_KEY = "tact-dev-placeholder-api-key"
FAKE_ACCOUNT_ID = "00000000-0000-0000-0000-000000000000"


def _jwt(claims: dict[str, object]) -> str:
    def encode(value: dict[str, object]) -> str:
        content = json.dumps(value, separators=(",", ":")).encode()
        return base64.urlsafe_b64encode(content).decode().rstrip("=")

    return f"{encode({'alg': 'none', 'typ': 'JWT'})}.{encode(claims)}.signature"


FAKE_ACCESS_TOKEN = _jwt(
    {
        "aud": ["https://api.openai.com/v1", "https://chatgpt.com/backend-api"],
        "exp": 4_102_444_800,
        "sub": "tact-development-proxy",
    }
)
FAKE_ID_TOKEN = _jwt(
    {
        "exp": 4_102_444_800,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": FAKE_ACCOUNT_ID,
            "chatgpt_plan_type": "plus",
        },
        "sub": "tact-development-proxy",
    }
)


@dataclass(frozen=True)
class SecretReplacement:
    file: str
    placeholder: str
    header: str


@dataclass(frozen=True)
class AuthRoute:
    host: str
    paths: tuple[str, ...]
    secrets: tuple[SecretReplacement, ...]
    methods: tuple[str, ...] = ("GET", "POST")


AUTH_ROUTES = {
    "api-key": AuthRoute(
        host="api.openai.com",
        paths=("/v1", "/v1/*"),
        secrets=(SecretReplacement("api-key", FAKE_API_KEY, "Authorization"),),
    ),
    "chatgpt": AuthRoute(
        host="chatgpt.com",
        paths=("/backend-api/codex", "/backend-api/codex/*"),
        secrets=(
            SecretReplacement("access-token", FAKE_ACCESS_TOKEN, "Authorization"),
            SecretReplacement("account-id", FAKE_ACCOUNT_ID, "chatgpt-account-id"),
        ),
    ),
}


@dataclass
class Credentials:
    """Authentication material owned by the launcher and cleared after projection."""

    mode: str
    access_token: bytearray
    account_id: bytearray | None = None
    source_path: Path | None = None

    def __enter__(self) -> Credentials:
        return self

    def __exit__(self, *_: object) -> None:
        self.clear()

    def __repr__(self) -> str:
        return f"Credentials(mode={self.mode!r}, [REDACTED])"

    def clear(self) -> None:
        self.access_token[:] = b"\0" * len(self.access_token)
        if self.account_id is not None:
            self.account_id[:] = b"\0" * len(self.account_id)


def default_codex_auth_file(environment: Mapping[str, str] = os.environ) -> Path:
    explicit = environment.get("TACT_AUTH_FILE")
    if explicit:
        return Path(explicit).expanduser().absolute()
    codex_home = environment.get("CODEX_HOME")
    if codex_home:
        return Path(codex_home).expanduser().absolute() / "auth.json"
    return Path.home() / ".codex" / "auth.json"


def default_tact_config_file(environment: Mapping[str, str] = os.environ) -> Path:
    explicit = environment.get("TACT_CONFIG")
    if explicit:
        return Path(explicit).expanduser().absolute()
    tact_home = environment.get("TACT_HOME")
    if tact_home:
        return Path(tact_home).expanduser().absolute() / "config.toml"
    return Path.home() / ".tact" / "config.toml"


def select_credentials(
    requested_mode: str,
    environment: Mapping[str, str] = os.environ,
    *,
    minimum_lifetime_seconds: int = DEFAULT_MINIMUM_LIFETIME_SECONDS,
) -> Credentials:
    if requested_mode not in {"auto", "api-key", "chatgpt"}:
        raise ValueError(f"unsupported authentication mode: {requested_mode}")

    if requested_mode == "chatgpt":
        return read_codex_credentials(
            default_codex_auth_file(environment), minimum_lifetime_seconds
        )
    if requested_mode == "api-key":
        return read_api_key(environment)

    subscription_error: RuntimeError | None = None
    try:
        return read_codex_credentials(
            default_codex_auth_file(environment), minimum_lifetime_seconds
        )
    except RuntimeError as error:
        subscription_error = error

    try:
        return read_api_key(environment)
    except RuntimeError as api_error:
        raise RuntimeError(
            "automatic authentication found neither valid ChatGPT credentials nor an API key; "
            f"ChatGPT: {subscription_error}; API key: {api_error}"
        ) from api_error


def read_api_key(environment: Mapping[str, str] = os.environ) -> Credentials:
    key_file = environment.get("OPENAI_API_KEY_FILE")
    if key_file:
        key_path = Path(key_file).expanduser().absolute()
        content = _read_private_file(key_path, "API key")
        while content.endswith((b"\n", b"\r")):
            del content[-1]
        if not content or any(value in b"\r\n\0" for value in content):
            content[:] = b"\0" * len(content)
            raise RuntimeError("the API key file does not contain one non-empty line")
        return Credentials(
            "api-key", content, source_path=key_path.resolve(strict=True)
        )

    value = environment.get("OPENAI_API_KEY", "")
    if not value or any(character in value for character in "\r\n\0"):
        raise RuntimeError("OPENAI_API_KEY is not set to a valid single-line value")
    return Credentials("api-key", bytearray(value.encode()))


def read_codex_credentials(
    path: Path,
    minimum_lifetime_seconds: int = DEFAULT_MINIMUM_LIFETIME_SECONDS,
) -> Credentials:
    content = _read_private_file(path, "Codex credentials")
    try:
        auth_mode = _json_string(content, b"auth_mode")
        if auth_mode is not None and auth_mode != b"chatgpt":
            raise RuntimeError(f"Codex is not logged in with ChatGPT in {path}")

        access_token = _json_string(content, b"access_token")
        account_id = _json_string(content, b"account_id")
        if access_token is None:
            raise RuntimeError(f"Codex credentials at {path} have no access_token")
        if account_id is None:
            access_token[:] = b"\0" * len(access_token)
            raise RuntimeError(f"Codex credentials at {path} have no account_id")

        try:
            expires_at = _jwt_expiration(access_token)
        except Exception:
            access_token[:] = b"\0" * len(access_token)
            account_id[:] = b"\0" * len(account_id)
            raise
        if expires_at is None or expires_at < time.time() + minimum_lifetime_seconds:
            access_token[:] = b"\0" * len(access_token)
            account_id[:] = b"\0" * len(account_id)
            raise RuntimeError(
                "the Codex access token expires too soon; use Codex to refresh the login "
                "and try again"
            )
        return Credentials(
            "chatgpt", access_token, account_id, source_path=path.resolve(strict=True)
        )
    finally:
        content[:] = b"\0" * len(content)


def fake_auth_document() -> bytes:
    document = {
        "OPENAI_API_KEY": None,
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": FAKE_ID_TOKEN,
            "access_token": FAKE_ACCESS_TOKEN,
            "refresh_token": "not-a-real-refresh-token",
            "account_id": FAKE_ACCOUNT_ID,
        },
        "last_refresh": "2025-01-01T00:00:00.000Z",
    }
    return json.dumps(document, separators=(",", ":")).encode()


def write_auth_projection(
    credentials: Credentials, private_directory: Path, public_directory: Path
) -> None:
    if credentials.mode == "api-key":
        _write_private_file(private_directory / "api-key", credentials.access_token)
    elif credentials.mode == "chatgpt" and credentials.account_id is not None:
        _write_private_file(private_directory / "access-token", credentials.access_token)
        _write_private_file(private_directory / "account-id", credentials.account_id)
    else:
        raise RuntimeError("incomplete authentication credentials")

    write_public_file(public_directory / "auth.json", fake_auth_document())
    proxy = proxy_configuration(credentials.mode)
    _write_private_file(
        private_directory / "proxy.yaml",
        bytearray(json.dumps(proxy, separators=(",", ":")).encode()),
    )


def proxy_configuration(mode: str) -> dict[str, object]:
    route = AUTH_ROUTES.get(mode)
    if route is None:
        raise ValueError(f"unsupported authentication mode: {mode}")

    return {
        "dns": {"enabled": False},
        "proxy": {
            "tunnel_listen": "127.0.0.1:8080",
            # Omitted listeners bind all interfaces on their default ports.
            "http_listen": "127.0.0.1:0",
            "https_listen": "127.0.0.1:0",
        },
        "metrics": {"listen": "127.0.0.1:0"},
        "tls": {
            "mode": "mitm",
            "ca_cert": str(PROXY_AUTH_DIRECTORY / "ca.crt"),
            "ca_key": str(PROXY_AUTH_DIRECTORY / "ca.key"),
        },
        "transforms": [
            {
                "name": "secrets",
                "config": {
                    "secrets": [
                        _secret_swap(route, replacement)
                        for replacement in route.secrets
                    ]
                },
            }
        ],
        "log": {"level": "warn"},
    }


def project_local_agent_files(
    public_directory: Path,
    environment: Mapping[str, str] = os.environ,
    *,
    include_instructions: bool = True,
    include_skills: bool = True,
    additional_skill_roots: Iterable[Path] = (),
) -> None:
    host_home = Path(environment.get("HOME", Path.home())).expanduser()
    codex_home = environment.get("CODEX_HOME")
    host_codex_home = (
        Path(codex_home).expanduser() if codex_home else host_home / ".codex"
    )
    if include_instructions:
        _project_instructions(host_codex_home, public_directory / "codex")
    if not include_skills:
        return

    destination = public_directory / "codex" / "skills"
    roots = [host_codex_home / "skills", host_home / ".agents" / "skills"]
    roots.extend(additional_skill_roots)
    for index, root in enumerate(roots):
        _project_skill_root(Path(root), destination / f"root-{index}")


def _secret_swap(
    route: AuthRoute, replacement: SecretReplacement
) -> dict[str, object]:
    return {
        "source": {
            "type": "file",
            "path": str(PROXY_AUTH_DIRECTORY / replacement.file),
        },
        "replace": {
            "proxy_value": replacement.placeholder,
            "match_headers": [replacement.header],
            "require": True,
        },
        "rules": [
            {
                "host": route.host,
                "methods": list(route.methods),
                "paths": list(route.paths),
            }
        ],
    }


def _read_private_file(path: Path, description: str) -> bytearray:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise RuntimeError(f"failed to read local {description} at {path}") from error

    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise RuntimeError(f"{description} is not a regular file: {path}")
        if os.name == "posix" and metadata.st_mode & 0o077:
            raise RuntimeError(f"local {description} is not private: {path}")
        content = bytearray(MAX_AUTH_FILE_BYTES + 1)
        with os.fdopen(descriptor, "rb", buffering=0) as file:
            bytes_read = file.readinto(content)
        del content[bytes_read:]
        descriptor = -1
    finally:
        if descriptor >= 0:
            os.close(descriptor)

    if len(content) > MAX_AUTH_FILE_BYTES:
        content[:] = b"\0" * len(content)
        raise RuntimeError(f"{description} file is unexpectedly large: {path}")
    return content


def _json_string(document: bytearray, field: bytes) -> bytearray | None:
    pattern = rb'"' + re.escape(field) + rb'"\s*:\s*"([^"\\]+)"'
    match = re.search(pattern, document)
    if match is None:
        return None
    start, end = match.span(1)
    if start == end:
        return None
    return document[start:end]


def _jwt_expiration(token: bytearray) -> int | float | None:
    separators = [index for index, value in enumerate(token) if value == ord(".")]
    if len(separators) != 2:
        raise RuntimeError("Codex access token is not a JWT")
    payload = token[separators[0] + 1 : separators[1]]
    payload.extend(b"=" * (-len(payload) % 4))
    try:
        claims = json.loads(base64.urlsafe_b64decode(payload))
    except (ValueError, UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError("Codex access token has invalid claims") from error
    if not isinstance(claims, dict):
        return None
    expires_at = claims.get("exp")
    return expires_at if isinstance(expires_at, (int, float)) else None


def _write_private_file(path: Path, value: bytearray) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as file:
            file.write(value)
        descriptor = -1
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def write_public_file(path: Path, value: bytes) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(descriptor, "wb") as file:
            file.write(value)
        descriptor = -1
    finally:
        if descriptor >= 0:
            os.close(descriptor)
    path.chmod(0o444)


def _project_skill_root(source: Path, destination: Path) -> None:
    if not source.exists():
        destination.mkdir(mode=0o700, parents=True)
        return
    _copy_safe_tree(source.expanduser().resolve(strict=True), destination)


def _copy_safe_tree(source: Path, destination: Path) -> None:
    metadata = source.lstat()
    if not stat.S_ISDIR(metadata.st_mode):
        raise RuntimeError(f"skill root is not a real directory: {source}")
    destination.mkdir(mode=0o700, parents=True)
    for child in sorted(source.iterdir(), key=lambda path: path.name):
        child_metadata = child.lstat()
        target = destination / child.name
        if stat.S_ISLNK(child_metadata.st_mode):
            continue
        if stat.S_ISDIR(child_metadata.st_mode):
            _copy_safe_tree(child, target)
            continue
        if not stat.S_ISREG(child_metadata.st_mode):
            raise RuntimeError(f"skill projection refuses special file: {child}")
        if child_metadata.st_size > MAX_PROJECTED_FILE_BYTES:
            raise RuntimeError(f"skill projection refuses oversized file: {child}")
        shutil.copyfile(child, target, follow_symlinks=False)
        target.chmod(0o444)


def _project_instructions(source_home: Path, destination: Path) -> None:
    source = next(
        (
            candidate
            for candidate in (
                source_home / "AGENTS.override.md",
                source_home / "AGENTS.md",
            )
            if candidate.exists()
        ),
        None,
    )
    if source is None:
        return
    try:
        source = source.resolve(strict=True)
        metadata = source.stat()
    except OSError as error:
        raise RuntimeError(f"instruction projection cannot resolve file: {source}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise RuntimeError(f"instruction projection refuses non-regular file: {source}")
    if metadata.st_size > MAX_PROJECTED_FILE_BYTES:
        raise RuntimeError(f"instruction projection refuses oversized file: {source}")
    write_public_file(destination / "AGENTS.md", source.read_bytes())
