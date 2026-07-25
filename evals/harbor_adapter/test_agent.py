"""Contracts for the Tact Harbor adapter."""

import asyncio
import base64
import json
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

import yaml
from harbor.models.agent.context import AgentContext
from harbor.environments.docker.docker import DockerEnvironment

from harbor_adapter.agent import MODEL, TactAgent
from harbor_adapter.credentials import (
    FAKE_ACCESS_TOKEN,
    fake_auth_document,
    read_codex_credentials,
)
from harbor_adapter.evidence import (
    EvidencePolicy,
    _child_metrics,
    _root_metrics,
    _validate_orchestration,
)
from harbor_adapter.installation import cli_tools_install_command
from harbor_adapter.iron_proxy import (
    IRON_PROXY_IMAGE,
    REMOTE_AUTH_FILE,
    REMOTE_CA_FILE,
    LocalCodexAuthProxy,
    _run_docker,
)


def jwt(claims: dict[str, object]) -> str:
    def encode(value: dict[str, object]) -> str:
        content = json.dumps(value, separators=(",", ":")).encode()
        return base64.urlsafe_b64encode(content).decode().rstrip("=")

    return f"{encode({'alg': 'none'})}.{encode(claims)}.signature"


def auth_document(suffix: str, expires_at: float | None = None) -> bytes:
    access_token = jwt(
        {
            "exp": expires_at or time.time() + 24 * 60 * 60,
            "suffix": suffix,
        }
    )
    id_token = jwt(
        {
            "https://api.openai.com/auth": {
                "account_id": "account-1",
                "fedramp": False,
            },
            "suffix": suffix,
        }
    )
    return json.dumps(
        {
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": id_token,
                "access_token": access_token,
                "refresh_token": f"refresh-{suffix}",
                "account_id": "account-1",
            },
        }
    ).encode()


class ArgumentContractTests(unittest.TestCase):
    def test_run_uses_tact_headless_mode_and_retains_orchestration(self) -> None:
        agent = object.__new__(TactAgent)
        agent._effort = "low"
        agent._reasoning_mode = "standard"
        agent._max_subagents = 8
        agent._web_search = False
        agent._image_generation = False
        agent._append_instructions = None
        agent._auth_proxy = LocalCodexAuthProxy("unused")

        arguments = agent._run_arguments("- inspect the workspace")

        self.assertEqual(arguments[0], "/installed-agent/tact")
        self.assertIn("chatgpt", arguments)
        self.assertIn(REMOTE_AUTH_FILE, arguments)
        self.assertIn("/app", arguments)
        self.assertIn("/logs/agent/orchestration.jsonl", arguments)
        self.assertEqual(arguments[-2:], ["--", "- inspect the workspace"])

    def test_model_name_must_match_the_tact_build(self) -> None:
        self.assertEqual(TactAgent._api_model_name(f"openai/{MODEL}"), MODEL)
        self.assertEqual(TactAgent._api_model_name(MODEL), MODEL)

    def test_user_environment_cannot_override_subscription_auth(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = TactAgent(
                logs_dir=Path(directory),
                model_name=f"openai/{MODEL}",
                extra_env={
                    "OPENAI_API_KEY": "must-not-enter-task",
                    "HTTPS_PROXY": "http://wrong-proxy",
                    "UNRELATED": "retained",
                },
            )

        self.assertNotIn("OPENAI_API_KEY", agent.extra_env)
        self.assertNotIn("HTTPS_PROXY", agent.extra_env)
        self.assertEqual(agent.extra_env["UNRELATED"], "retained")

    def test_tool_installer_supports_common_task_images(self) -> None:
        command = cli_tools_install_command(install_node=True)

        for package_manager in ("apk add", "apt-get install", "yum install"):
            self.assertIn(package_manager, command)
        for tool in ("bash", "curl", "rg", "node", "npm"):
            self.assertIn(f"command -v {tool}", command)

class InstallContractTests(unittest.TestCase):
    def test_local_binary_is_uploaded_and_made_executable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "tact"
            binary.write_bytes(b"binary")
            agent = object.__new__(TactAgent)
            agent._binary_path = binary
            agent._install_node = False
            agent._auth_proxy = SimpleNamespace(require_local_docker=lambda _: None)
            agent.exec_as_root = AsyncMock()
            environment = SimpleNamespace(upload_file=AsyncMock())

            asyncio.run(agent.install(environment))

            environment.upload_file.assert_awaited_once_with(
                binary, "/installed-agent/tact"
            )
        self.assertEqual(
            agent.exec_as_root.await_args_list[-1].args[1],
            "chmod 0755 /installed-agent/tact",
        )


class CredentialContractTests(unittest.TestCase):
    def test_remote_environments_are_rejected_before_reading_credentials(self) -> None:
        proxy = LocalCodexAuthProxy("/path/that/does/not/exist")

        with self.assertRaisesRegex(RuntimeError, "local Docker"):
            proxy.require_local_docker(SimpleNamespace())

    def test_remote_docker_daemon_is_rejected(self) -> None:
        proxy = LocalCodexAuthProxy("/path/that/does/not/exist")
        environment = object.__new__(DockerEnvironment)
        environment._is_windows_container = False

        with patch(
            "harbor_adapter.iron_proxy._docker_endpoint",
            return_value="ssh://remote-builder",
        ):
            with self.assertRaisesRegex(RuntimeError, "local Docker socket"):
                proxy.require_local_docker(environment)

    def test_task_receives_only_fake_auth_and_the_public_ca(self) -> None:
        async def exercise(
            auth_file: Path,
        ) -> tuple[list[tuple[str, bytes]], list[list[str]]]:
            proxy = LocalCodexAuthProxy(auth_file, safe_bind_root="/safe-trial")
            environment = object.__new__(DockerEnvironment)
            environment._is_windows_container = False
            environment._run_docker_compose_command = AsyncMock(
                return_value=SimpleNamespace(stdout="task-container\n")
            )
            uploaded: list[tuple[str, bytes]] = []

            async def upload(source: Path, target: str) -> None:
                uploaded.append((target, source.read_bytes()))

            environment.upload_file = AsyncMock(side_effect=upload)
            exec_as_agent = AsyncMock()
            exec_as_root = AsyncMock()
            docker_commands: list[list[str]] = []

            async def docker(arguments: list[str], **_: object) -> SimpleNamespace:
                docker_commands.append(arguments)
                if "generate-ca" in arguments:
                    output = Path(
                        arguments[arguments.index("--volume") + 1].split(":", 1)[0]
                    )
                    (output / "ca.crt").write_text("public-ca", encoding="utf-8")
                    (output / "ca.key").write_text("private-ca", encoding="utf-8")
                if arguments[0] == "inspect":
                    inspection = [
                        {
                            "HostConfig": {
                                "Privileged": False,
                                "PidMode": "",
                                "NetworkMode": "default",
                                "IpcMode": "private",
                                "CapAdd": None,
                                "Devices": [],
                            },
                            "Mounts": [
                                {
                                    "Type": "bind",
                                    "Source": "/safe-trial/agent",
                                    "Destination": "/logs/agent",
                                }
                            ],
                        }
                    ]
                    return SimpleNamespace(
                        returncode=0,
                        stdout=json.dumps(inspection),
                    )
                return SimpleNamespace(returncode=0, stdout="")

            with (
                patch(
                    "harbor_adapter.iron_proxy._docker_endpoint",
                    return_value="unix:///var/run/docker.sock",
                ),
                patch("harbor_adapter.iron_proxy._run_docker", side_effect=docker),
            ):
                async with proxy.running(
                    environment,
                    exec_as_agent=exec_as_agent,
                    exec_as_root=exec_as_root,
                ):
                    pass
            return uploaded, docker_commands

        with tempfile.TemporaryDirectory() as directory:
            auth_file = Path(directory) / "auth.json"
            source = auth_document("real")
            auth_file.write_bytes(source)
            auth_file.chmod(0o600)
            uploaded, docker_commands = asyncio.run(exercise(auth_file))

        self.assertEqual(
            [target for target, _ in uploaded], [REMOTE_AUTH_FILE, REMOTE_CA_FILE]
        )
        task_bytes = b"".join(content for _, content in uploaded)
        source_tokens = json.loads(source)["tokens"]
        self.assertNotIn(source_tokens["access_token"].encode(), task_bytes)
        self.assertNotIn(source_tokens["refresh_token"].encode(), task_bytes)
        self.assertEqual(json.loads(uploaded[0][1]), json.loads(fake_auth_document()))

        start = next(command for command in docker_commands if "--detach" in command)
        self.assertIn("container:task-container", start)
        self.assertIn(f"{IRON_PROXY_IMAGE}", start)
        self.assertIn(":/run/tact-auth:ro", " ".join(start))
        self.assertNotIn(source_tokens["access_token"], " ".join(start))
        self.assertEqual(docker_commands[-1][0:2], ["rm", "--force"])

    def test_access_token_must_cover_the_next_trial(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_file = Path(directory) / "auth.json"
            auth_file.write_bytes(auth_document("expired", time.time() - 1))
            auth_file.chmod(0o600)
            proxy = LocalCodexAuthProxy(auth_file)

            with self.assertRaisesRegex(RuntimeError, "expires too soon"):
                read_codex_credentials(
                    proxy.path,
                    minimum_lifetime_seconds=proxy.minimum_lifetime_seconds,
                )

    def test_owned_credentials_are_redacted_and_cleared(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            auth_file = Path(directory) / "auth.json"
            auth_file.write_bytes(auth_document("mutable"))
            auth_file.chmod(0o600)
            credentials = read_codex_credentials(auth_file)

            self.assertEqual(repr(credentials), "CodexCredentials([REDACTED])")
            self.assertTrue(credentials.access_token.startswith(b"ey"))
            credentials.clear()

        self.assertEqual(set(credentials.access_token), {0})
        self.assertEqual(set(credentials.account_id), {0})

    def test_proxy_removal_failure_is_reported(self) -> None:
        async def exercise() -> None:
            proxy = LocalCodexAuthProxy("unused")
            exec_as_root = AsyncMock()
            with patch(
                "harbor_adapter.iron_proxy._run_docker",
                AsyncMock(side_effect=RuntimeError("removal failed")),
            ):
                with self.assertRaisesRegex(RuntimeError, "removal failed"):
                    await proxy._cleanup(
                        SimpleNamespace(),
                        container_name="proxy-container",
                        exec_as_root=exec_as_root,
                    )
            exec_as_root.assert_awaited_once()

        asyncio.run(exercise())

    def test_unsafe_task_container_is_rejected_before_credentials_are_read(
        self,
    ) -> None:
        async def exercise() -> None:
            inspection = [
                {
                    "HostConfig": {
                        "Privileged": False,
                        "PidMode": "",
                        "NetworkMode": "default",
                        "IpcMode": "private",
                        "CapAdd": None,
                        "Devices": [],
                    },
                    "Mounts": [
                        {
                            "Type": "bind",
                            "Destination": "/var/run/docker.sock",
                        }
                    ],
                }
            ]
            result = SimpleNamespace(stdout=json.dumps(inspection))
            with patch(
                "harbor_adapter.iron_proxy._run_docker",
                AsyncMock(return_value=result),
            ):
                with self.assertRaisesRegex(RuntimeError, "task bind mounts"):
                    await LocalCodexAuthProxy("unused")._require_isolated_task("task")

        asyncio.run(exercise())

    def test_docker_startup_finishes_before_cancellation_returns(self) -> None:
        async def exercise() -> None:
            started = asyncio.Event()
            finish = asyncio.Event()

            class Process:
                returncode = 0

                async def communicate(self) -> tuple[bytes, bytes]:
                    started.set()
                    await finish.wait()
                    return b"container-id", b""

            with patch(
                "harbor_adapter.iron_proxy.asyncio.create_subprocess_exec",
                AsyncMock(return_value=Process()),
            ):
                command = asyncio.create_task(
                    _run_docker(["run", "--detach"], timeout_seconds=30)
                )
                await started.wait()
                command.cancel()
                await asyncio.sleep(0)
                self.assertFalse(command.done())
                finish.set()
                with self.assertRaises(asyncio.CancelledError):
                    await command

        asyncio.run(exercise())

    def test_proxy_config_swaps_mock_headers_only_on_codex_routes(self) -> None:
        proxy = LocalCodexAuthProxy("unused")
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            proxy._write_public_files(path)
            config = yaml.safe_load((path / "proxy.yaml").read_text())

        codex_rule = {
            "host": "chatgpt.com",
            "methods": ["GET", "POST"],
            "paths": [
                "/backend-api/codex",
                "/backend-api/codex/*",
            ],
        }
        allowlist = config["transforms"][0]["config"]["rules"]
        secrets = config["transforms"][1]["config"]["secrets"]
        self.assertEqual(
            allowlist,
            [
                {"host": "chatgpt.com", "methods": ["CONNECT"]},
                codex_rule,
            ],
        )
        self.assertEqual(secrets[0]["replace"]["proxy_value"], FAKE_ACCESS_TOKEN)
        self.assertTrue(secrets[0]["replace"]["require"])
        self.assertEqual(secrets[0]["source"]["type"], "file")
        self.assertTrue(all(secret["rules"] == [codex_rule] for secret in secrets))


class MetricsContractTests(unittest.TestCase):
    def test_root_and_child_metrics_combine_cache_usage(self) -> None:
        root = _root_metrics(
            [{"type": "tool.call"}, {"type": "run.completed"}],
            {
                "model_calls": 2,
                "cost_usd": 0.5,
                "usage": {
                    "input_tokens": 100,
                    "cached_input_tokens": 60,
                    "output_tokens": 20,
                    "total_tokens": 120,
                },
            },
        )
        child = _child_metrics(
            {
                "child_metrics": {
                    "model_calls": 3,
                    "tool_calls": 4,
                    "cost_usd": 0.75,
                    "usage": {
                        "input_tokens": 200,
                        "cached_input_tokens": 150,
                        "output_tokens": 30,
                        "total_tokens": 230,
                    },
                    "warmup_usage": {},
                }
            }
        )

        total = root.plus(child)

        self.assertEqual(total.model_calls, 5)
        self.assertEqual(total.tool_calls, 5)
        self.assertEqual(total.cost_usd, 1.25)
        self.assertEqual(total.usage["input_tokens"], 300)
        self.assertEqual(total.usage["cached_input_tokens"], 210)
        self.assertEqual(total.usage["total_tokens"], 350)

    def test_malformed_metrics_are_rejected(self) -> None:
        with self.assertRaisesRegex(RuntimeError, "root metrics.model_calls"):
            _root_metrics(
                [],
                {
                    "model_calls": "one",
                    "cost_usd": None,
                    "usage": {
                        "input_tokens": 1,
                        "cached_input_tokens": 0,
                        "output_tokens": 1,
                        "total_tokens": 2,
                    },
                },
            )

    def test_orchestration_requires_a_final_clean_summary(self) -> None:
        records = [
            {
                "protocol_version": 1,
                "sequence": 1,
                "root_session_id": "root",
                "type": "orchestration.completed",
                "agents_started": 1,
                "active_agent_ids": [],
                "failed_agent_ids": [],
                "child_metrics": {},
            }
        ]

        policy = EvidencePolicy(
            minimum_subagents=1,
            fail_on_subagent_error=False,
            require_wait=False,
        )
        summary = _validate_orchestration(records, "root", policy)

        self.assertEqual(summary["agents_started"], 1)
        records[0]["active_agent_ids"] = [1]
        with self.assertRaisesRegex(RuntimeError, "left active subagents"):
            _validate_orchestration(records, "root", policy)

    def test_complete_failed_run_writes_atif_and_aggregate_context(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            logs = Path(directory)
            (logs / "input.jsonl").write_text(
                '{"instruction":"inspect"}\n', encoding="utf-8"
            )
            events = [
                {
                    "protocol_version": 1,
                    "request_id": "root",
                    "seq": 1,
                    "type": "run.started",
                    "payload": {},
                },
                {
                    "protocol_version": 1,
                    "request_id": "root",
                    "seq": 2,
                    "type": "assistant.message",
                    "payload": {"text": "done"},
                },
                {
                    "protocol_version": 1,
                    "request_id": "root",
                    "seq": 3,
                    "type": "run.failed",
                    "payload": {
                        "model": MODEL,
                        "effort": "low",
                        "model_calls": 1,
                        "cost_usd": 0.5,
                        "usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 50,
                            "output_tokens": 20,
                            "total_tokens": 120,
                        },
                    },
                },
            ]
            orchestration = [
                {
                    "protocol_version": 1,
                    "sequence": 1,
                    "root_session_id": "root",
                    "type": "orchestration.completed",
                    "outcome": "completed",
                    "agents_started": 0,
                    "active_agent_ids": [],
                    "failed_agent_ids": [],
                    "agents": [],
                    "child_metrics": {
                        "model_calls": 2,
                        "tool_calls": 3,
                        "cost_usd": 0.75,
                        "usage": {
                            "input_tokens": 200,
                            "cached_input_tokens": 100,
                            "output_tokens": 30,
                            "total_tokens": 230,
                        },
                        "warmup_usage": {},
                    },
                }
            ]
            for name, values in (
                ("events.jsonl", events),
                ("orchestration.jsonl", orchestration),
            ):
                (logs / name).write_text(
                    "".join(json.dumps(value) + "\n" for value in values),
                    encoding="utf-8",
                )
            context = AgentContext()
            agent = object.__new__(TactAgent)
            agent.logs_dir = logs
            agent._minimum_subagents = 0
            agent._fail_on_subagent_error = False
            agent._require_wait = False
            agent._run_interrupted = False
            agent._run_failed = True
            agent._post_run_validation_failed = False
            agent.version = lambda: "test"

            agent.populate_context_post_run(context)

            trajectory = json.loads((logs / "trajectory.json").read_text())
            self.assertEqual(context.n_input_tokens, 300)
            self.assertEqual(context.n_cache_tokens, 150)
            self.assertEqual(context.n_output_tokens, 50)
            self.assertEqual(context.cost_usd, 1.25)
            self.assertEqual(trajectory["final_metrics"]["extra"]["model_calls"], 3)


class ConfigurationContractTests(unittest.TestCase):
    def test_eval_python_project_is_self_contained(self) -> None:
        evals_directory = Path(__file__).resolve().parents[1]
        repository = evals_directory.parent

        self.assertTrue((evals_directory / "pyproject.toml").is_file())
        self.assertTrue((evals_directory / "uv.lock").is_file())
        self.assertFalse((repository / "pyproject.toml").exists())
        self.assertFalse((repository / "uv.lock").exists())

    def test_terminal_bench_is_pinned_and_disables_external_tools(self) -> None:
        evals_directory = Path(__file__).resolve().parents[1]
        config = yaml.safe_load(
            (evals_directory / "terminal-bench.yaml").read_text(encoding="utf-8")
        )

        self.assertEqual(config["datasets"][0]["ref"], "6")
        self.assertEqual(config["environment"]["type"], "docker")
        self.assertNotIn("auth_mode", config["agents"][0]["kwargs"])
        self.assertNotIn("env", config["agents"][0])
        self.assertIs(config["agents"][0]["kwargs"]["web_search"], False)
        self.assertIs(config["agents"][0]["kwargs"]["image_generation"], False)

    def test_harbor_recipe_passes_dataset_for_cli_filters(self) -> None:
        repository = Path(__file__).resolve().parents[2]
        justfile = (repository / "justfile").read_text(encoding="utf-8")
        harbor_recipe = justfile.split("harbor-eval *args='':", 1)[1].split(
            "# Browse retained Harbor jobs",
            1,
        )[0]

        self.assertIn(
            "--dataset terminal-bench/terminal-bench-2-1@6",
            harbor_recipe,
        )


if __name__ == "__main__":
    unittest.main()
