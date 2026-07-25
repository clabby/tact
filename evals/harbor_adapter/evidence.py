"""Validate Tact event evidence and convert it to Harbor's ATIF format."""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from harbor.models.agent.context import AgentContext
from harbor.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)
from harbor.utils.trajectory_utils import format_trajectory_json


PROTOCOL_VERSION = 1
TERMINAL_EVENTS = {"run.completed", "run.failed"}
RUN_METRIC_FIELDS = (
    "connection_attempts",
    "websocket_reconnects",
    "connection_duration_ns",
    "model_duration_ns",
    "warmup_duration_ns",
    "tool_work_duration_ns",
    "tool_wall_duration_ns",
)
USAGE_FIELDS = (
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
    "total_tokens",
)


@dataclass(frozen=True)
class EvidencePolicy:
    minimum_subagents: int
    fail_on_subagent_error: bool
    require_wait: bool


@dataclass(frozen=True)
class RunMetrics:
    model_calls: int
    tool_calls: int
    cost_usd: float | None
    usage: dict[str, int]
    warmup_usage: dict[str, int]

    def plus(self, other: RunMetrics) -> RunMetrics:
        costs = [cost for cost in (self.cost_usd, other.cost_usd) if cost is not None]
        return RunMetrics(
            model_calls=self.model_calls + other.model_calls,
            tool_calls=self.tool_calls + other.tool_calls,
            cost_usd=sum(costs) if costs else None,
            usage=_sum_usage(self.usage, other.usage),
            warmup_usage=_sum_usage(self.warmup_usage, other.warmup_usage),
        )


def populate_context(
    *,
    logs_dir: Path,
    context: AgentContext,
    agent_name: str,
    agent_version: str,
    policy: EvidencePolicy,
) -> None:
    prompts = _read_jsonl(logs_dir / "input.jsonl")
    if len(prompts) != 1 or not isinstance(prompts[0].get("instruction"), str):
        raise RuntimeError("input.jsonl must contain one prompt")

    events = _read_jsonl(logs_dir / "events.jsonl")
    terminal = _validate_events(events)
    orchestration = _validate_orchestration(
        _read_jsonl(logs_dir / "orchestration.jsonl"),
        events[0]["request_id"],
        policy,
    )
    terminal_payload = terminal["payload"]
    root_metrics = _root_metrics(events, terminal_payload)
    child_metrics = _child_metrics(orchestration)
    total_metrics = root_metrics.plus(child_metrics)
    wait_calls = _successful_wait_calls(events)
    if policy.require_wait and orchestration["agents_started"] and wait_calls == 0:
        raise RuntimeError(
            "Tact started subagents without a successful wait_agent result"
        )

    reasoning = "".join(
        event["payload"].get("text", "")
        for event in events
        if event["type"] == "reasoning.summary.delta"
        and isinstance(event["payload"].get("text"), str)
    )
    tool_calls = _atif_tool_calls(events)
    observations = _atif_observations(events, tool_calls)
    message = _final_message(events)
    orchestration_summary = {
        "agents_started": orchestration["agents_started"],
        "failed_agent_ids": orchestration["failed_agent_ids"],
        "active_agent_ids": orchestration["active_agent_ids"],
        "successful_wait_calls": wait_calls,
        "child_metrics": orchestration["child_metrics"],
    }
    root_runtime = {field: terminal_payload.get(field) for field in RUN_METRIC_FIELDS}
    root_runtime["warmup_usage"] = root_metrics.warmup_usage

    trajectory = Trajectory(
        session_id=events[0]["request_id"],
        agent=Agent(
            name=agent_name,
            version=agent_version,
            model_name=terminal_payload.get("model"),
            extra={
                "transport": terminal_payload.get("transport"),
                "orchestration": orchestration_summary,
            },
        ),
        steps=[
            Step(step_id=1, source="user", message=prompts[0]["instruction"]),
            Step(
                step_id=2,
                source="agent",
                message=message,
                model_name=terminal_payload.get("model"),
                reasoning_effort=terminal_payload.get("effort"),
                reasoning_content=reasoning or None,
                tool_calls=tool_calls or None,
                observation=(
                    Observation(results=observations) if observations else None
                ),
                metrics=(
                    Metrics(
                        prompt_tokens=root_metrics.usage["input_tokens"],
                        completion_tokens=root_metrics.usage["output_tokens"],
                        cached_tokens=root_metrics.usage["cached_input_tokens"],
                        cost_usd=root_metrics.cost_usd,
                        extra=root_runtime,
                    )
                    if root_metrics.model_calls
                    else None
                ),
                llm_call_count=root_metrics.model_calls,
                extra={
                    "terminal_event_type": terminal["type"],
                    "terminal_payload": terminal_payload,
                },
            ),
        ],
        notes=None,
        final_metrics=FinalMetrics(
            total_prompt_tokens=total_metrics.usage["input_tokens"],
            total_completion_tokens=total_metrics.usage["output_tokens"],
            total_cached_tokens=total_metrics.usage["cached_input_tokens"],
            total_cost_usd=total_metrics.cost_usd,
            total_steps=2,
            extra={
                "model_calls": total_metrics.model_calls,
                "tool_calls": total_metrics.tool_calls,
                "root_metrics": _metrics_dict(root_metrics),
                "child_metrics": _metrics_dict(child_metrics),
                "orchestration": orchestration_summary,
            },
        ),
    )
    (logs_dir / "trajectory.json").write_text(
        format_trajectory_json(trajectory.to_json_dict()), encoding="utf-8"
    )

    context.n_input_tokens = total_metrics.usage["input_tokens"]
    context.n_cache_tokens = total_metrics.usage["cached_input_tokens"]
    context.n_output_tokens = total_metrics.usage["output_tokens"]
    context.cost_usd = total_metrics.cost_usd
    context.metadata = {
        "protocol_version": PROTOCOL_VERSION,
        "terminal_event_type": terminal["type"],
        "model": terminal_payload.get("model"),
        "effort": terminal_payload.get("effort"),
        "transport": terminal_payload.get("transport"),
        "root_metrics": _metrics_dict(root_metrics),
        "child_metrics": _metrics_dict(child_metrics),
        "total_metrics": _metrics_dict(total_metrics),
        "orchestration": orchestration_summary,
    }


def _validate_events(events: list[dict[str, Any]]) -> dict[str, Any]:
    if not events or not isinstance(events[0].get("request_id"), str):
        raise RuntimeError("events.jsonl must contain a request ID")
    request_id = events[0]["request_id"]
    for sequence, event in enumerate(events, start=1):
        if (
            event.get("protocol_version") != PROTOCOL_VERSION
            or event.get("request_id") != request_id
            or event.get("seq") != sequence
            or not isinstance(event.get("type"), str)
            or not isinstance(event.get("payload"), dict)
        ):
            raise RuntimeError(f"invalid Tact event at sequence {sequence}")
    terminals = [event for event in events if event["type"] in TERMINAL_EVENTS]
    if len(terminals) != 1 or terminals[0] is not events[-1]:
        raise RuntimeError("events.jsonl must end with exactly one terminal event")
    return terminals[0]


def _validate_orchestration(
    records: list[dict[str, Any]], root_session_id: str, policy: EvidencePolicy
) -> dict[str, Any]:
    if not records:
        raise RuntimeError("orchestration.jsonl must not be empty")
    for sequence, record in enumerate(records, start=1):
        if (
            record.get("protocol_version") != PROTOCOL_VERSION
            or record.get("sequence") != sequence
            or record.get("root_session_id") != root_session_id
            or not isinstance(record.get("type"), str)
        ):
            raise RuntimeError(
                f"invalid Tact orchestration record at sequence {sequence}"
            )
    summary = records[-1]
    if summary["type"] != "orchestration.completed":
        raise RuntimeError("orchestration.jsonl is missing its cleanup summary")
    if summary.get("active_agent_ids"):
        raise RuntimeError("Tact left active subagents after cleanup")
    agents_started = summary.get("agents_started")
    if not isinstance(agents_started, int):
        raise RuntimeError("orchestration summary has no agent count")
    if agents_started < policy.minimum_subagents:
        raise RuntimeError(
            f"expected at least {policy.minimum_subagents} subagents, "
            f"observed {agents_started}"
        )
    if policy.fail_on_subagent_error and summary.get("failed_agent_ids"):
        raise RuntimeError("one or more Tact subagents failed")
    if not isinstance(summary.get("child_metrics"), dict):
        raise RuntimeError("orchestration summary has no child metrics")
    return summary


def _atif_tool_calls(events: list[dict[str, Any]]) -> list[ToolCall]:
    calls = []
    for event in events:
        if event["type"] != "tool.call":
            continue
        payload = event["payload"]
        arguments = payload.get("arguments")
        if not isinstance(arguments, dict):
            arguments = {"raw": arguments}
        calls.append(
            ToolCall(
                tool_call_id=str(payload.get("call_id", "")),
                function_name=str(payload.get("tool", "")),
                arguments=arguments,
                extra={"model_call_index": payload.get("model_call_index")},
            )
        )
    return calls


def _atif_observations(
    events: list[dict[str, Any]], calls: list[ToolCall]
) -> list[ObservationResult]:
    call_ids = {call.tool_call_id for call in calls}
    observations = []
    for event in events:
        if event["type"] != "tool.result":
            continue
        payload = event["payload"]
        call_id = str(payload.get("call_id", ""))
        if call_id not in call_ids:
            continue
        observations.append(
            ObservationResult(
                source_call_id=call_id,
                content=json.dumps(
                    payload.get("result", payload), separators=(",", ":")
                ),
                extra={
                    "status": payload.get("status"),
                    "duration_ns": payload.get("duration_ns"),
                },
            )
        )
    return observations


def _read_jsonl(path: Path) -> list[dict[str, Any]]:
    deadline = time.monotonic() + 30.0
    while True:
        try:
            text = path.read_text(encoding="utf-8")
            values = [json.loads(line) for line in text.splitlines() if line.strip()]
            break
        except OSError as error:
            if time.monotonic() >= deadline:
                raise RuntimeError(
                    f"failed to read JSONL from {path}: {error}"
                ) from error
            time.sleep(0.05)
        except json.JSONDecodeError as error:
            if text.endswith(("\n", "\r")) or time.monotonic() >= deadline:
                raise RuntimeError(
                    f"failed to read JSONL from {path}: {error}"
                ) from error
            time.sleep(0.05)
    if not all(isinstance(value, dict) for value in values):
        raise RuntimeError(f"all JSONL values in {path} must be objects")
    return values


def _root_metrics(
    events: list[dict[str, Any]], terminal_payload: dict[str, Any]
) -> RunMetrics:
    return RunMetrics(
        model_calls=_required_integer(terminal_payload, "model_calls", "root metrics"),
        tool_calls=sum(event["type"] == "tool.call" for event in events),
        cost_usd=_optional_number(terminal_payload, "cost_usd", "root metrics"),
        usage=_usage(terminal_payload.get("usage"), "root usage", required=True),
        warmup_usage=_usage(
            terminal_payload.get("warmup_usage"), "root warmup usage", required=False
        ),
    )


def _child_metrics(summary: dict[str, Any]) -> RunMetrics:
    metrics = summary["child_metrics"]
    return RunMetrics(
        model_calls=_required_integer(metrics, "model_calls", "child metrics"),
        tool_calls=_required_integer(metrics, "tool_calls", "child metrics"),
        cost_usd=_optional_number(metrics, "cost_usd", "child metrics"),
        usage=_usage(metrics.get("usage"), "child usage", required=True),
        warmup_usage=_usage(
            metrics.get("warmup_usage"), "child warmup usage", required=False
        ),
    )


def _successful_wait_calls(events: list[dict[str, Any]]) -> int:
    wait_ids = {
        str(event["payload"].get("call_id", ""))
        for event in events
        if event["type"] == "tool.call"
        and event["payload"].get("tool") == "wait_agent"
    }
    return sum(
        event["type"] == "tool.result"
        and str(event["payload"].get("call_id", "")) in wait_ids
        and event["payload"].get("status") not in {"error", "failed"}
        for event in events
    )


def _final_message(events: list[dict[str, Any]]) -> str:
    messages = [event for event in events if event["type"] == "assistant.message"]
    for event in reversed(messages):
        payload = event["payload"]
        if payload.get("phase") != "final_answer":
            continue
        if isinstance(payload.get("text"), str):
            return payload["text"]
    for event in reversed(messages):
        text = event["payload"].get("text")
        if isinstance(text, str):
            return text
    return "Tact emitted no assistant message."


def _usage(value: Any, label: str, *, required: bool) -> dict[str, int]:
    if value is None and not required:
        value = {}
    if not isinstance(value, dict):
        raise RuntimeError(f"{label} must be an object")

    required_fields = {
        "input_tokens",
        "cached_input_tokens",
        "output_tokens",
        "total_tokens",
    }
    usage = {}
    for field in USAGE_FIELDS:
        field_value = value.get(field, 0)
        if field in required_fields and field not in value and required:
            raise RuntimeError(f"{label} is missing {field}")
        if (
            not isinstance(field_value, int)
            or isinstance(field_value, bool)
            or field_value < 0
        ):
            raise RuntimeError(f"{label}.{field} must be a non-negative integer")
        usage[field] = field_value
    return usage


def _sum_usage(left: dict[str, int], right: dict[str, int]) -> dict[str, int]:
    return {field: left.get(field, 0) + right.get(field, 0) for field in USAGE_FIELDS}


def _metrics_dict(metrics: RunMetrics) -> dict[str, Any]:
    return {
        "model_calls": metrics.model_calls,
        "tool_calls": metrics.tool_calls,
        "cost_usd": metrics.cost_usd,
        "usage": metrics.usage,
        "warmup_usage": metrics.warmup_usage,
    }


def _required_integer(value: dict[str, Any], field: str, label: str) -> int:
    field_value = value.get(field)
    if (
        not isinstance(field_value, int)
        or isinstance(field_value, bool)
        or field_value < 0
    ):
        raise RuntimeError(f"{label}.{field} must be a non-negative integer")
    return field_value


def _optional_number(value: dict[str, Any], field: str, label: str) -> float | None:
    field_value = value.get(field)
    if field_value is None:
        return None
    if (
        not isinstance(field_value, (int, float))
        or isinstance(field_value, bool)
        or field_value < 0
    ):
        raise RuntimeError(f"{label}.{field} must be null or a non-negative number")
    return float(field_value)
