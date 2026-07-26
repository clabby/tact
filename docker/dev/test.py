from __future__ import annotations

import base64
import importlib.util
import json
import os
import platform
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from pathlib import Path
from unittest import mock


def run_smoke_checks() -> None:
    manifest_path = Path("/usr/local/share/tact-dev/tools.toml")
    with manifest_path.open("rb") as manifest_file:
        manifest = tomllib.load(manifest_file)

    required = {
        "bun",
        "bunx",
        "cargo",
        "cargo-binstall",
        "cc",
        "c++",
        "clang",
        "curl",
        "eu-readelf",
        "git",
        "ld",
        "ld.lld",
        "lld",
        "lldb",
        "llvm-config",
        "node",
        "npm",
        "python3",
        "rustc",
        "rustfmt",
        "tact",
        "uv",
        "uvx",
    }
    required.update(tool["executable"] for tool in manifest["tools"].values())
    missing = sorted(command for command in required if shutil.which(command) is None)
    if missing:
        raise RuntimeError(f"missing executables: {', '.join(missing)}")

    expected_versions = {
        "bun": manifest["bun"]["version"],
        "node": f'v{manifest["node"]["version"]}',
        "python3": manifest["python"]["version"],
        "uv": manifest["uv"]["version"],
    }
    actual_versions = {
        "bun": _output(["bun", "--version"]),
        "node": _output(["node", "--version"]),
        "python3": platform.python_version(),
        "uv": _output(["uv", "--version"]).split()[1],
    }
    if actual_versions != expected_versions:
        raise RuntimeError(
            f"runtime version mismatch: expected {expected_versions}, got {actual_versions}"
        )

    rust_version = _output(["rustc", "--version"])
    if not rust_version.startswith(f'rustc {manifest["rust"]["stable"]} '):
        raise RuntimeError(f"unexpected rustc version: {rust_version}")
    if manifest["rust"]["nightly"] not in _output(["rustup", "toolchain", "list"]):
        raise RuntimeError(f'missing Rust toolchain {manifest["rust"]["nightly"]}')

    llvm_major = manifest["debian"]["llvm_major"]
    for command in ("clang", "llvm-config", "ld.lld", "lldb"):
        version = _output([command, "--version"])
        if llvm_major not in version:
            raise RuntimeError(f"{command} is not LLVM {llvm_major}: {version.splitlines()[0]}")

    for tool in manifest["tools"].values():
        executable = tool["executable"]
        command = [executable, "--version"]
        if executable.startswith("cargo-"):
            command = ["cargo", executable.removeprefix("cargo-"), "--version"]
        subprocess.run(
            command,
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.STDOUT,
        )

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        c_source = root / "main.c"
        c_source.write_text("int main(void) { return 0; }\n")
        for compiler in ("cc", "clang"):
            binary = root / compiler
            subprocess.run([compiler, str(c_source), "-o", str(binary)], check=True)
            subprocess.run([binary], check=True)
        for command in ("readelf", "eu-readelf"):
            subprocess.run(
                [command, "-h", root / "cc"],
                check=True,
                stdout=subprocess.DEVNULL,
            )

        cpp_source = root / "main.cc"
        cpp_source.write_text(
            '#include <iostream>\nint main() { std::cout << "ok\\n"; }\n'
        )
        subprocess.run(["c++", str(cpp_source), "-o", root / "cpp"], check=True)
        if _output([root / "cpp"]) != "ok":
            raise RuntimeError("C++ fixture produced unexpected output")

        rust_source = root / "main.rs"
        rust_source.write_text('fn main() { println!("ok"); }\n')
        subprocess.run(["rustc", rust_source, "-o", root / "rust"], check=True)
        if _output([root / "rust"]) != "ok":
            raise RuntimeError("Rust fixture produced unexpected output")

    runtime_checks = (
        (["python3", "-c", 'print("ok")'], "Python"),
        (["node", "-e", 'console.log("ok")'], "Node"),
        (["bun", "-e", 'console.log("ok")'], "Bun"),
    )
    for command, name in runtime_checks:
        if _output(command) != "ok":
            raise RuntimeError(f"{name} fixture produced unexpected output")

    architecture = (_output(["dpkg", "--print-architecture"]), platform.machine())
    if architecture not in {("amd64", "x86_64"), ("arm64", "aarch64")}:
        raise RuntimeError(f"architecture mismatch: {architecture[0]}:{architecture[1]}")
    print(f"development toolchain smoke checks passed ({architecture[0]})")


def _output(command: list[str | Path]) -> str:
    return subprocess.check_output(command, text=True).strip()


if __name__ == "__main__" and sys.argv[1:] == ["--smoke"]:
    run_smoke_checks()
    raise SystemExit(0)


DEV_DIRECTORY = Path(__file__).resolve().parent
sys.path.insert(0, str(DEV_DIRECTORY))

import config  # noqa: E402

RUN_SPEC = importlib.util.spec_from_file_location("tact_dev_run", DEV_DIRECTORY / "run.py")
assert RUN_SPEC is not None and RUN_SPEC.loader is not None
dev_run = importlib.util.module_from_spec(RUN_SPEC)
RUN_SPEC.loader.exec_module(dev_run)


def jwt(expires_at: int) -> str:
    def encode(value: dict[str, object]) -> str:
        return base64.urlsafe_b64encode(
            json.dumps(value, separators=(",", ":")).encode()
        ).decode().rstrip("=")

    return f"{encode({'alg': 'none'})}.{encode({'exp': expires_at})}.signature"


def write_private(path: Path, value: bytes) -> None:
    path.write_bytes(value)
    path.chmod(0o600)


def write_chatgpt_auth(path: Path, *, expires_at: int | None = None) -> str:
    token = jwt(expires_at or int(time.time()) + 7200)
    document = {
        "auth_mode": "chatgpt",
        "tokens": {"access_token": token, "account_id": "real-account"},
    }
    write_private(path, json.dumps(document).encode())
    return token


class CredentialTests(unittest.TestCase):
    def test_codex_home_selects_auth_file(self) -> None:
        self.assertEqual(
            config.default_codex_auth_file({"CODEX_HOME": "/custom/codex"}),
            Path("/custom/codex/auth.json"),
        )
        self.assertEqual(
            config.default_codex_auth_file(
                {"TACT_AUTH_FILE": "/explicit/auth.json", "CODEX_HOME": "/ignored"}
            ),
            Path("/explicit/auth.json"),
        )

    def test_tact_config_precedence(self) -> None:
        self.assertEqual(
            config.default_tact_config_file(
                {"TACT_CONFIG": "/explicit.toml", "TACT_HOME": "/ignored"}
            ),
            Path("/explicit.toml"),
        )
        self.assertEqual(
            config.default_tact_config_file({"TACT_HOME": "/tact"}),
            Path("/tact/config.toml"),
        )

    def test_read_valid_chatgpt_credentials(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json"
            token = write_chatgpt_auth(path)
            credentials = config.read_codex_credentials(path)
            self.assertEqual(credentials.mode, "chatgpt")
            self.assertEqual(credentials.access_token, token.encode())
            self.assertEqual(credentials.account_id, b"real-account")
            self.assertEqual(repr(credentials), "Credentials(mode='chatgpt', [REDACTED])")

    def test_chatgpt_credentials_are_cleared(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json"
            write_chatgpt_auth(path)
            credentials = config.read_codex_credentials(path)
            with credentials:
                token_length = len(credentials.access_token)
                account_length = len(credentials.account_id or b"")
            self.assertEqual(credentials.access_token, bytearray(token_length))
            self.assertEqual(credentials.account_id, bytearray(account_length))

    def test_expiring_chatgpt_token_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json"
            write_chatgpt_auth(path, expires_at=int(time.time()) + 10)
            with self.assertRaisesRegex(RuntimeError, "expires too soon"):
                config.read_codex_credentials(path)

    def test_non_jwt_chatgpt_token_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json"
            document = {
                "auth_mode": "chatgpt",
                "tokens": {"access_token": "opaque", "account_id": "account"},
            }
            write_private(path, json.dumps(document).encode())
            with self.assertRaisesRegex(RuntimeError, "not a JWT"):
                config.read_codex_credentials(path)

    def test_public_codex_file_is_rejected(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX permissions required")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth.json"
            write_chatgpt_auth(path)
            path.chmod(0o644)
            with self.assertRaisesRegex(RuntimeError, "not private"):
                config.read_codex_credentials(path)

    def test_symlinked_codex_file_is_rejected(self) -> None:
        if not hasattr(os, "O_NOFOLLOW"):
            self.skipTest("O_NOFOLLOW required")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "real.json"
            link = root / "auth.json"
            write_chatgpt_auth(target)
            link.symlink_to(target)
            with self.assertRaisesRegex(RuntimeError, "failed to read"):
                config.read_codex_credentials(link)

    def test_api_key_environment_and_private_file(self) -> None:
        environment_credentials = config.read_api_key({"OPENAI_API_KEY": "secret"})
        self.assertEqual(environment_credentials.access_token, b"secret")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "key"
            write_private(path, b"file-secret\n")
            file_credentials = config.read_api_key(
                {"OPENAI_API_KEY": "ignored", "OPENAI_API_KEY_FILE": str(path)}
            )
            self.assertEqual(file_credentials.access_token, b"file-secret")
            self.assertEqual(file_credentials.source_path, path.resolve())

    def test_api_key_rejects_multiline_value(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "single-line"):
            config.read_api_key({"OPENAI_API_KEY": "one\ntwo"})

    def test_auto_prefers_chatgpt_then_falls_back_to_api_key(self) -> None:
        chatgpt = config.Credentials("chatgpt", bytearray(b"token"), bytearray(b"id"))
        with mock.patch.object(config, "read_codex_credentials", return_value=chatgpt):
            selected = config.select_credentials("auto", {"OPENAI_API_KEY": "key"})
        self.assertIs(selected, chatgpt)

        with mock.patch.object(
            config, "read_codex_credentials", side_effect=RuntimeError("invalid")
        ):
            selected = config.select_credentials("auto", {"OPENAI_API_KEY": "key"})
        self.assertEqual(selected.mode, "api-key")
        self.assertEqual(selected.access_token, b"key")

    def test_auto_reports_both_failures(self) -> None:
        with mock.patch.object(
            config, "read_codex_credentials", side_effect=RuntimeError("expired")
        ):
            with self.assertRaisesRegex(RuntimeError, "ChatGPT: expired"):
                config.select_credentials("auto", {})

    def test_fake_auth_contains_no_real_credentials(self) -> None:
        rendered = config.fake_auth_document()
        self.assertNotIn(b"secret-sentinel", rendered)
        document = json.loads(rendered)
        self.assertEqual(document["auth_mode"], "chatgpt")
        self.assertEqual(document["tokens"]["access_token"], config.FAKE_ACCESS_TOKEN)


class ProxyConfigurationTests(unittest.TestCase):
    def test_chatgpt_proxy_configuration_matches_hardened_rules(self) -> None:
        document = config.proxy_configuration("chatgpt")
        self.assertEqual(document["dns"], {"enabled": False})
        self.assertEqual(
            document["proxy"],
            {
                "tunnel_listen": "127.0.0.1:8080",
                "http_listen": "127.0.0.1:0",
                "https_listen": "127.0.0.1:0",
            },
        )
        self.assertEqual(document["metrics"], {"listen": "127.0.0.1:0"})
        self.assertEqual(document["tls"]["mode"], "mitm")
        self.assertEqual([item["name"] for item in document["transforms"]], ["secrets"])
        secrets = document["transforms"][0]["config"]["secrets"]
        self.assertEqual(len(secrets), 2)
        self.assertTrue(all(secret["replace"]["require"] for secret in secrets))
        self.assertEqual(
            {secret["replace"]["match_headers"][0] for secret in secrets},
            {"Authorization", "chatgpt-account-id"},
        )

    def test_api_proxy_replaces_credentials_for_openai_v1(self) -> None:
        document = config.proxy_configuration("api-key")
        secret = document["transforms"][0]["config"]["secrets"][0]
        self.assertEqual(secret["source"]["path"], "/run/tact-auth/api-key")
        self.assertEqual(secret["replace"]["proxy_value"], config.FAKE_API_KEY)
        self.assertTrue(secret["replace"]["require"])
        self.assertEqual(
            secret["rules"],
            [
                {
                    "host": "api.openai.com",
                    "methods": ["GET", "POST"],
                    "paths": ["/v1", "/v1/*"],
                }
            ],
        )

    def test_auth_projection_permissions_and_secret_boundaries(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX permissions required")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            private = root / "private"
            public = root / "public"
            private.mkdir(mode=0o700)
            public.mkdir(mode=0o700)
            secret = b"secret-sentinel"
            credentials = config.Credentials("api-key", bytearray(secret))
            config.write_auth_projection(credentials, private, public)
            self.assertEqual((private / "api-key").read_bytes(), secret)
            self.assertEqual(stat.S_IMODE((private / "api-key").stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE((private / "proxy.yaml").stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE((public / "auth.json").stat().st_mode), 0o444)
            self.assertNotIn(secret, (private / "proxy.yaml").read_bytes())
            self.assertNotIn(secret, (public / "auth.json").read_bytes())


class LocalAgentFileTests(unittest.TestCase):
    def test_codex_instructions_and_skills_are_projected_without_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex = root / "codex"
            skill = codex / "skills" / "example"
            skill.mkdir(parents=True)
            (codex / "AGENTS.md").write_text("normal")
            (codex / "AGENTS.override.md").write_text("override")
            (codex / "auth.json").write_text("secret-sentinel")
            (skill / "SKILL.md").write_text("skill instructions")
            (skill / "escape").symlink_to(codex / "auth.json")
            agents_skill = root / ".agents" / "skills" / "shared"
            agents_skill.mkdir(parents=True)
            (agents_skill / "SKILL.md").write_text("shared instructions")
            public = root / "public"
            public.mkdir()
            config.project_local_agent_files(
                public, {"CODEX_HOME": str(codex), "HOME": str(root)}
            )
            self.assertEqual((public / "codex/AGENTS.md").read_text(), "override")
            projected_skill = public / "codex/skills/root-0/example/SKILL.md"
            self.assertEqual(projected_skill.read_text(), "skill instructions")
            self.assertEqual(stat.S_IMODE(projected_skill.stat().st_mode), 0o444)
            self.assertFalse((public / "codex/auth.json").exists())
            self.assertFalse((public / "codex/skills/root-0/example/escape").exists())
            self.assertEqual(
                (public / "codex/skills/root-1/shared/SKILL.md").read_text(),
                "shared instructions",
            )
            self.assertFalse((public / "config.toml").exists())

    def test_symlinked_instruction_file_is_resolved(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex = root / "codex"
            codex.mkdir()
            target = root / "shared-agents.md"
            target.write_text("shared instructions")
            (codex / "AGENTS.md").symlink_to(target)
            public = root / "public"
            public.mkdir()
            config.project_local_agent_files(public, {"CODEX_HOME": str(codex)})
            self.assertEqual(
                (public / "codex/AGENTS.md").read_text(), "shared instructions"
            )


class LauncherTests(unittest.TestCase):
    def test_missing_config_is_created_for_the_writable_state_mount(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "state" / "config.toml"
            state_directory, config_name = dev_run.prepare_tact_state(
                {"TACT_CONFIG": str(config_path)}
            )
            self.assertEqual(state_directory, config_path.parent.resolve())
            self.assertEqual(config_name, "config.toml")
            self.assertEqual(config_path.read_text(), "")
            self.assertEqual(stat.S_IMODE(config_path.stat().st_mode), 0o600)

    def test_symlinked_config_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target.toml"
            target.write_text("")
            link = root / "config.toml"
            link.symlink_to(target)
            with self.assertRaisesRegex(RuntimeError, "symlink"):
                dev_run.prepare_tact_state({"TACT_CONFIG": str(link)})

    def test_credential_file_cannot_overlap_a_writable_mount(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            workspace = Path(directory).resolve()
            source = workspace / "api-key"
            credentials = config.Credentials(
                "api-key", bytearray(b"secret"), source_path=source
            )
            with self.assertRaisesRegex(RuntimeError, "writable container mount"):
                dev_run.ensure_credentials_not_mounted(
                    credentials, (workspace, workspace / ".tact")
                )

    def test_tact_state_is_a_writable_host_bind_mount(self) -> None:
        compose = (DEV_DIRECTORY / "compose.yaml").read_text()
        self.assertIn("TACT_HOME: /run/tact-state", compose)
        self.assertIn('TACT_CONFIG: "/run/tact-state/${TACT_DEV_CONFIG_NAME:', compose)
        self.assertIn(
            'source: "${TACT_DEV_STATE_DIR:?set TACT_DEV_STATE_DIR}"', compose
        )
        self.assertIn("target: /run/tact-state", compose)

    def test_parser_keeps_arguments_after_separator(self) -> None:
        arguments = dev_run.parser().parse_args(
            ["--auth", "api-key", "--", "--thinking", "high"]
        )
        self.assertEqual(arguments.auth, "api-key")
        self.assertEqual(arguments.tact_arguments, ["--", "--thinking", "high"])

    def test_local_docker_validation_rejects_remote_endpoint_without_info(self) -> None:
        with mock.patch.object(dev_run, "_run_docker") as run_docker:
            with self.assertRaisesRegex(RuntimeError, "local Docker socket"):
                dev_run.require_local_linux_docker({"DOCKER_HOST": "tcp://remote:2375"})
        run_docker.assert_not_called()

    def test_local_docker_validation_checks_context_then_linux(self) -> None:
        results = [
            subprocess.CompletedProcess([], 0, "unix:///socket\n", ""),
            subprocess.CompletedProcess([], 0, "linux\n", ""),
        ]
        with mock.patch.object(dev_run, "_run_docker", side_effect=results) as run_docker:
            dev_run.require_local_linux_docker({"PATH": "/bin"})
        self.assertEqual(run_docker.call_count, 2)
        self.assertEqual(run_docker.call_args_list[0].args[0][0:2], ["context", "inspect"])
        self.assertEqual(run_docker.call_args_list[1].args[0][0], "info")

    def test_compose_environment_is_an_allowlist_without_secrets(self) -> None:
        result = dev_run._compose_environment(
            {
                "PATH": "/bin",
                "DOCKER_HOST": "unix:///socket",
                "OPENAI_API_KEY": "secret-sentinel",
                "TACT_DEV_IMAGE": "custom-tact:local",
                "UNRELATED_SECRET": "also-secret",
            },
            workspace=Path("/workspace"),
            state_directory=Path("/host/.tact"),
            state_config_name="config.toml",
            private_directory=Path("/private"),
            public_directory=Path("/public"),
            auth_mode="api-key",
            iron_proxy_image="iron:test@sha256:abc",
        )
        self.assertEqual(result["TACT_DEV_AUTH"], "api-key")
        self.assertEqual(result["TACT_DEV_WORKSPACE"], "/workspace")
        self.assertEqual(result["TACT_DEV_STATE_DIR"], "/host/.tact")
        self.assertEqual(result["TACT_DEV_CONFIG_NAME"], "config.toml")
        self.assertEqual(result["TACT_DEV_IRON_IMAGE"], "iron:test@sha256:abc")
        self.assertEqual(result["TACT_DEV_IMAGE"], "custom-tact:local")
        self.assertNotIn("OPENAI_API_KEY", result)
        self.assertNotIn("UNRELATED_SECRET", result)
        self.assertNotIn("secret-sentinel", json.dumps(result))

    def test_compose_override_preserves_arguments_without_shell_parsing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = dev_run._write_compose_override(root, ["--thinking", "x high", "$TOKEN"])
            document = json.loads(path.read_text())
            self.assertEqual(
                document["services"]["tact-dev"]["command"],
                ["--thinking", "x high", "$TOKEN"],
            )
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o444)

    def test_shell_override_replaces_entrypoint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = dev_run._write_compose_override(Path(directory), [], shell=True)
            service = json.loads(path.read_text())["services"]["tact-dev"]
            self.assertEqual(
                service,
                {"command": [], "environment": {"TACT_DEV_SHELL": "1"}},
            )

    def test_clean_treats_missing_volume_as_success(self) -> None:
        result = subprocess.CompletedProcess([], 1, "", "Error: no such volume: tact-dev-state")
        with mock.patch.object(dev_run.subprocess, "run", return_value=result) as run_command:
            self.assertEqual(dev_run._clean_persistent_volume({"PATH": "/bin"}), 0)
        command = run_command.call_args.args[0]
        self.assertEqual(command, ["docker", "volume", "rm", "tact-dev-state"])

    def test_ca_generation_uses_no_network_and_private_permissions(self) -> None:
        if os.name != "posix":
            self.skipTest("POSIX permissions required")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def generate(arguments, _environment, *, timeout):
                self.assertIn("none", arguments)
                self.assertEqual(timeout, dev_run.DOCKER_TIMEOUT_SECONDS)
                (root / "ca.crt").write_text("certificate")
                (root / "ca.key").write_text("private key")
                return subprocess.CompletedProcess([], 0, "", "")

            with mock.patch.object(dev_run, "_run_docker", side_effect=generate):
                dev_run.generate_proxy_ca(
                    root, {"PATH": "/bin"}, "iron:test@sha256:abc"
                )
            self.assertEqual(stat.S_IMODE((root / "ca.crt").stat().st_mode), 0o600)
            self.assertEqual(stat.S_IMODE((root / "ca.key").stat().st_mode), 0o600)

    def test_docker_is_validated_before_credentials_are_selected(self) -> None:
        arguments = dev_run.parser().parse_args(["--auth", "api-key"])
        order: list[str] = []
        with mock.patch.object(
            dev_run, "require_local_linux_docker", side_effect=lambda _env: order.append("docker")
        ), mock.patch.object(
            dev_run, "_ensure_persistent_volume"
        ), mock.patch.object(
            dev_run.config,
            "select_credentials",
            side_effect=lambda *_args: order.append("credentials"),
        ), mock.patch.object(
            dev_run.Path, "resolve", side_effect=RuntimeError("stop after validation")
        ):
            with self.assertRaisesRegex(RuntimeError, "stop"):
                dev_run.run(arguments, {})
        self.assertEqual(order, ["docker"])

    def test_compose_signal_handlers_are_restored(self) -> None:
        process = mock.Mock()
        process.wait.return_value = 23
        process.poll.return_value = None
        previous = {signal.SIGINT: signal.getsignal(signal.SIGINT), signal.SIGTERM: signal.getsignal(signal.SIGTERM)}
        with mock.patch.object(
            dev_run.subprocess, "Popen", return_value=process
        ) as popen:
            result = dev_run._run_compose(["docker", "compose"], {"PATH": "/bin"})
        self.assertEqual(result, 23)
        self.assertEqual(
            popen.call_args.args[0],
            ["docker", "compose", "run", "--rm", "tact-dev"],
        )
        self.assertEqual(signal.getsignal(signal.SIGINT), previous[signal.SIGINT])
        self.assertEqual(signal.getsignal(signal.SIGTERM), previous[signal.SIGTERM])


class DevelopmentImageTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        subprocess.run(
            [
                "docker",
                "buildx",
                "bake",
                "--progress",
                "plain",
                "--file",
                "docker/docker-bake.hcl",
                "dev",
            ],
            cwd=DEV_DIRECTORY.parents[1],
            check=True,
        )

    def test_development_image(self) -> None:
        subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--interactive",
                "--entrypoint",
                "python3",
                "tact-dev:local",
                "-",
                "--smoke",
            ],
            input=Path(__file__).read_bytes(),
            check=True,
        )
        metadata = _output(
            [
                "docker",
                "image",
                "inspect",
                "tact-dev:local",
                "--format",
                "{{.Config.User}}|{{index .Config.Entrypoint 0}}",
            ]
        )
        self.assertEqual(metadata, "tact|/usr/local/bin/tact-entrypoint")
        subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--entrypoint",
                "sh",
                "tact-dev:local",
                "-c",
                "test ! -e /usr/local/bin/tact-dev-smoke",
            ],
            check=True,
        )

    def test_new_host_state_loads_as_default_config(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            state_directory, config_name = dev_run.prepare_tact_state(
                {"TACT_HOME": directory}
            )
            subprocess.run(
                [
                    "docker",
                    "run",
                    "--rm",
                    "--user",
                    f"{os.getuid()}:{os.getgid()}",
                    "--volume",
                    f"{state_directory}:/run/tact-state",
                    "--env",
                    f"TACT_CONFIG=/run/tact-state/{config_name}",
                    "--entrypoint",
                    "tact",
                    "tact-dev:local",
                    "config",
                    "show",
                ],
                check=True,
                stdout=subprocess.DEVNULL,
            )


if __name__ == "__main__":
    unittest.main()
