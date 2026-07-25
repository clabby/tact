"""Read local Codex subscription auth and create harmless task credentials."""

from __future__ import annotations

import base64
import json
import os
import re
import stat
import time
from pathlib import Path


MAX_AUTH_FILE_BYTES = 1024 * 1024
DEFAULT_MINIMUM_LIFETIME_SECONDS = 60 * 60


def default_codex_auth_file() -> Path:
    codex_home = os.environ.get("CODEX_HOME")
    if codex_home:
        return Path(codex_home).expanduser().absolute() / "auth.json"
    return Path.home() / ".codex" / "auth.json"


def _jwt(claims: dict[str, object]) -> str:
    def encode(value: dict[str, object]) -> str:
        content = json.dumps(value, separators=(",", ":")).encode()
        return base64.urlsafe_b64encode(content).decode().rstrip("=")

    return f"{encode({'alg': 'none', 'typ': 'JWT'})}.{encode(claims)}.signature"


FAKE_ACCOUNT_ID = "00000000-0000-0000-0000-000000000000"
FAKE_ACCESS_TOKEN = _jwt(
    {
        "aud": [
            "https://api.openai.com/v1",
            "https://chatgpt.com/backend-api",
        ],
        "exp": 4_102_444_800,
        "sub": "tact-harbor-proxy",
    }
)
FAKE_ID_TOKEN = _jwt(
    {
        "exp": 4_102_444_800,
        "https://api.openai.com/auth": {
            "chatgpt_account_id": FAKE_ACCOUNT_ID,
            "chatgpt_plan_type": "plus",
        },
        "sub": "tact-harbor-proxy",
    }
)


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


class CodexCredentials:
    """Mutable access credentials that overwrite their owned buffers on exit."""

    def __init__(self, access_token: bytearray, account_id: bytearray) -> None:
        self.access_token = access_token
        self.account_id = account_id

    def __enter__(self) -> CodexCredentials:
        return self

    def __exit__(self, *_: object) -> None:
        self.clear()

    def __del__(self) -> None:
        self.clear()

    def __repr__(self) -> str:
        return "CodexCredentials([REDACTED])"

    def clear(self) -> None:
        self.access_token[:] = b"\0" * len(self.access_token)
        self.account_id[:] = b"\0" * len(self.account_id)


def read_codex_credentials(
    path: Path,
    minimum_lifetime_seconds: int = DEFAULT_MINIMUM_LIFETIME_SECONDS,
) -> CodexCredentials:
    content = _read_private_file(path)
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
        minimum_expiry = time.time() + minimum_lifetime_seconds
        if expires_at is None or expires_at < minimum_expiry:
            access_token[:] = b"\0" * len(access_token)
            account_id[:] = b"\0" * len(account_id)
            raise RuntimeError(
                "the Codex access token expires too soon for a Harbor trial; "
                "use Codex to refresh the login and try again"
            )
        return CodexCredentials(access_token, account_id)
    finally:
        content[:] = b"\0" * len(content)


def _read_private_file(path: Path) -> bytearray:
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise RuntimeError(
            f"failed to read local Codex credentials at {path}"
        ) from error

    try:
        metadata = os.fstat(descriptor)
        if not stat.S_ISREG(metadata.st_mode):
            raise RuntimeError(f"Codex credentials are not a regular file: {path}")
        if os.name == "posix" and metadata.st_mode & 0o077:
            raise RuntimeError(f"local Codex credentials are not private: {path}")
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
        raise RuntimeError(f"Codex credential file is unexpectedly large: {path}")
    return content


def _json_string(
    document: bytearray,
    field: bytes,
) -> bytearray | None:
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
