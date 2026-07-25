"""Install and run Tact inside a Harbor task environment."""

from __future__ import annotations

import asyncio
import json
import shlex
from pathlib import Path
from typing import Any

from harbor.agents.installed.base import BaseInstalledAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor_adapter.credentials import default_codex_auth_file
from harbor_adapter.evidence import EvidencePolicy, populate_context
from harbor_adapter.installation import cli_tools_install_command
from harbor_adapter.iron_proxy import REMOTE_AUTH_FILE, LocalCodexAuthProxy


MODEL = "gpt-5.6-sol"


class TactAgent(BaseInstalledAgent):
    """Upload one Tact binary, run it once, and retain its typed evidence."""

    SUPPORTS_ATIF = True
    _BINARY = "/installed-agent/tact"
    _EVENTS = "/logs/agent/events.jsonl"
    _EVENTS_TMP = "/logs/agent/events.jsonl.tmp"
    _ORCHESTRATION = "/logs/agent/orchestration.jsonl"
    _STDERR = "/logs/agent/stderr.log"

    def __init__(
        self,
        logs_dir: Path,
        binary_path: str | Path = ".tact/installed/tact",
        model_name: str | None = None,
        auth_file: str | Path | None = None,
        auth_minimum_lifetime_seconds: int = 60 * 60,
        effort: str = "low",
        reasoning_mode: str = "standard",
        max_subagents: int = 32,
        web_search: bool = False,
        image_generation: bool = False,
        install_node: bool = False,
        append_instructions: str | None = None,
        minimum_subagents: int = 0,
        fail_on_subagent_error: bool = False,
        require_wait: bool = False,
        extra_env: dict[str, str] | None = None,
        **kwargs: Any,
    ) -> None:
        agent_env = dict(extra_env or {})
        agent_env.pop("OPENAI_API_KEY", None)
        auth_proxy = LocalCodexAuthProxy(
            auth_file or default_codex_auth_file(),
            minimum_lifetime_seconds=auth_minimum_lifetime_seconds,
            safe_bind_root=logs_dir.parent,
        )
        for name in auth_proxy.agent_environment:
            agent_env.pop(name, None)
        super().__init__(
            logs_dir=logs_dir,
            model_name=model_name,
            extra_env=agent_env,
            **kwargs,
        )
        self._binary_path = Path(binary_path).resolve()
        if max_subagents < 1:
            raise ValueError("max_subagents must be at least 1")
        if auth_minimum_lifetime_seconds < 0:
            raise ValueError("auth_minimum_lifetime_seconds cannot be negative")
        if minimum_subagents < 0:
            raise ValueError("minimum_subagents cannot be negative")
        if minimum_subagents > max_subagents:
            raise ValueError("minimum_subagents cannot exceed max_subagents")

        self._model = self._api_model_name(model_name)
        if self._model != MODEL:
            raise ValueError(f"Tact supports only {MODEL}, got {self._model}")
        self._auth_proxy = auth_proxy
        self._effort = effort
        self._reasoning_mode = reasoning_mode
        self._max_subagents = max_subagents
        self._web_search = web_search
        self._image_generation = image_generation
        self._install_node = install_node
        self._append_instructions = append_instructions
        self._minimum_subagents = minimum_subagents
        self._fail_on_subagent_error = fail_on_subagent_error
        self._require_wait = require_wait
        self._run_interrupted = False
        self._run_failed = False
        self._post_run_validation_failed = False

    @staticmethod
    def name() -> str:
        return "tact"

    def get_version_command(self) -> str:
        return f"{self._BINARY} --version"

    async def install(self, environment: BaseEnvironment) -> None:
        self._auth_proxy.require_local_docker(environment)
        if not self._binary_path.is_file():
            raise RuntimeError(
                f"missing Tact binary at {self._binary_path}; "
                "run `just build-harbor-agent`"
            )
        await self.exec_as_root(
            environment,
            cli_tools_install_command(install_node=self._install_node),
            env={"DEBIAN_FRONTEND": "noninteractive"},
        )
        await environment.upload_file(self._binary_path, self._BINARY)
        await self.exec_as_root(environment, f"chmod 0755 {self._BINARY}")

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        self._run_interrupted = False
        self._run_failed = False
        self._post_run_validation_failed = False
        try:
            async with self._auth_proxy.running(
                environment,
                exec_as_agent=self.exec_as_agent,
                exec_as_root=self.exec_as_root,
            ):
                await self._run_to_completion(instruction, environment, context)
        except asyncio.CancelledError:
            self._run_interrupted = True
            raise
        except Exception:
            self._run_failed = True
            raise

    async def _run_to_completion(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        del context
        (self.logs_dir / "input.jsonl").write_text(
            json.dumps({"instruction": instruction}, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        arguments = self._run_arguments(instruction)
        agent_command = " ".join(shlex.quote(argument) for argument in arguments)
        command = (
            f"events_tmp={shlex.quote(self._EVENTS_TMP)}; "
            'rm -f "$events_tmp"; set +e; set -o pipefail; '
            f'{agent_command} 2> {shlex.quote(self._STDERR)} | tee "$events_tmp"; '
            'exit "$?"'
        )
        result = await self.exec_as_agent(
            environment,
            command,
            env=self._auth_proxy.agent_environment,
        )
        self._publish_events(result.stdout)

    def _run_arguments(self, prompt: str) -> list[str]:
        arguments = [
            self._BINARY,
            "--auth",
            "chatgpt",
            "--auth-file",
            REMOTE_AUTH_FILE,
        ]
        arguments.extend(
            [
                "--workspace",
                "/app",
                "--thinking",
                self._effort,
                "--reasoning-mode",
                self._reasoning_mode,
                "--max-subagents",
                str(self._max_subagents),
                "--web-search",
                str(self._web_search).lower(),
                "--image-generation",
                str(self._image_generation).lower(),
            ]
        )
        if self._append_instructions:
            arguments.extend(("--append-instructions", self._append_instructions))
        arguments.extend(
            (
                "run",
                "--orchestration-log",
                self._ORCHESTRATION,
                "--",
                prompt,
            )
        )
        return arguments

    def _classify_exec_error(self, command: str, result: Any) -> Exception:
        self._publish_events(result.stdout)
        return super()._classify_exec_error(command, result)

    def _publish_events(self, stdout: str | None) -> None:
        if stdout is None:
            return
        events = self.logs_dir / Path(self._EVENTS).name
        temporary = events.with_name(f"{events.name}.host.tmp")
        temporary.write_text(stdout, encoding="utf-8")
        temporary.replace(events)

    def populate_context_post_run(self, context: AgentContext) -> None:
        if self._post_run_validation_failed:
            return
        try:
            populate_context(
                logs_dir=self.logs_dir,
                context=context,
                agent_name=self.name(),
                agent_version=self.version() or "unknown",
                policy=EvidencePolicy(
                    minimum_subagents=self._minimum_subagents,
                    fail_on_subagent_error=self._fail_on_subagent_error,
                    require_wait=self._require_wait,
                ),
            )
        except Exception:
            if not (self._run_interrupted or self._run_failed):
                self._post_run_validation_failed = True
                raise
            self.logger.debug(
                "skipping strict Tact trajectory validation after an incomplete run",
                exc_info=True,
            )

    @staticmethod
    def _api_model_name(model_name: str | None) -> str:
        if model_name is None:
            return MODEL
        _, separator, api_model = model_name.partition("/")
        return api_model if separator else model_name
