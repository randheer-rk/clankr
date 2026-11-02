"""Clankr agent SDK (agent runtime ABI v1).

    from clankr import AgentApp, Permanent

    app = AgentApp()

    @app.task("research")              # workflow node id; "*" catches every node
    async def research(task, ctx):
        prev = await ctx.context("memory").get("progress")       # None if missing
        reply = await ctx.models.generate("default", [{"role": "user", "content": task.input["input"]["topic"]}])
        await ctx.context("memory").put("progress", {"summary": reply["text"]},
                                        if_version=prev["version"] if prev else 0)
        return {"summary": reply["text"]}

    app.run()

Handlers return a JSON-serializable result. Raising `Permanent` fails the task without retry;
any other exception fails it retryably. Delivery is at-least-once, so handlers must be
idempotent; pass `task.idempotency_key` to external systems that support deduplication.

The agent holds no credentials. Model and context calls go to the local node over
$CLANKR_SOCKET, and the node checks every call against the Agent spec.
"""

import asyncio
import contextvars
import json
import os
import struct
import sys
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Optional

ABI_MAJOR, ABI_MINOR = 1, 0
MAX_FRAME = 4 * 1024 * 1024
__version__ = "0.1.0"

__all__ = ["AgentApp", "Task", "Context", "Message", "ClankrError", "Permanent"]


class ClankrError(Exception):
    """An error returned by the node or control plane. `code` is a stable Clankr error code
    such as Forbidden, Conflict, QuotaExceeded or NotFound."""

    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code, self.message = code, message


class Permanent(Exception):
    """Raise from a handler to fail the task without retrying."""


@dataclass
class Task:
    run_id: str
    task: str
    node: str
    attempt: int
    idempotency_key: str
    input: Any


class _Conn:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.r, self.w = reader, writer
        self.lock = asyncio.Lock()

    async def send(self, frame: dict) -> None:
        body = json.dumps(frame).encode()
        if len(body) > MAX_FRAME:
            raise ValueError(f"frame of {len(body)} bytes exceeds {MAX_FRAME}")
        async with self.lock:
            self.w.write(struct.pack(">I", len(body)) + body)
            await self.w.drain()

    async def recv(self) -> dict:
        (n,) = struct.unpack(">I", await self.r.readexactly(4))
        if n > MAX_FRAME:
            raise ValueError(f"frame of {n} bytes exceeds {MAX_FRAME}")
        return json.loads(await self.r.readexactly(n))


class _Calls:
    """Request/reply over the socket, matched by call id."""

    def __init__(self, conn: _Conn):
        self.conn, self.next_id, self.pending = conn, 0, {}

    async def call(self, op: dict) -> Any:
        self.next_id += 1
        fut = asyncio.get_running_loop().create_future()
        self.pending[self.next_id] = fut
        await self.conn.send({"type": "call", "callId": self.next_id, "op": op})
        return await fut

    def resolve(self, frame: dict) -> None:
        fut = self.pending.pop(frame["callId"], None)
        if fut is None or fut.done():
            return
        if frame.get("error"):
            e = frame["error"]
            fut.set_exception(ClankrError(e.get("code", "Unknown"), e.get("message", "")))
        else:
            fut.set_result(frame.get("ok"))


class _Models:
    def __init__(self, calls: _Calls):
        self._calls = calls

    async def generate(self, claim: str, messages: list, system: Optional[str] = None, max_output_tokens: Optional[int] = None) -> dict:
        """Returns {"text", "stopReason", "usage": {"inputTokens", "outputTokens"}, "provider", "model"}."""
        req: dict = {"messages": messages}
        if system is not None:
            req["system"] = system
        if max_output_tokens is not None:
            req["maxOutputTokens"] = max_output_tokens
        return await self._calls.call({"op": "generate", "claim": claim, "request": req})


# the task a coroutine is working on, so ctx.tools can derive stable idempotency keys
_current: "contextvars.ContextVar[Optional[dict]]" = contextvars.ContextVar("clankr_task", default=None)


class _Tools:
    def __init__(self, calls: "_Calls"):
        self._calls = calls

    async def call(self, claim: str, tool: str, method: str, input: Any = None, idempotency_key: Optional[str] = None, approval: Optional[str] = None) -> dict:
        """Calls tool.method through a ToolClaim. Returns {"output", "outputSha256", "truncated",
        "durationMs", "toolVersion", "replayed"}. The default idempotency key is derived from the
        task and the call's position in it, so a retried task gets the recorded result instead
        of repeating the effect. High-risk methods spend an approval granted earlier in the run.
        Raises ClankrError: Forbidden (not granted / no approval), InvalidSpec (bad input),
        OutcomeUnknown (an unsafe call timed out; don't blindly retry)."""
        cur = _current.get()
        if idempotency_key is None:
            if cur is None:
                raise ClankrError("InvalidSpec", "pass idempotency_key when calling tools outside a task")
            cur["n"] += 1
            idempotency_key = f"{cur['task'].idempotency_key}:{tool}.{method}:{cur['n']}"
        body: dict = {"tool": tool, "method": method, "input": input if input is not None else {}, "idempotencyKey": idempotency_key}
        if approval is not None:
            body["approval"] = approval
        return await self._calls.call({"op": "toolCall", "claim": claim, "call": body})


class _ContextClaim:
    def __init__(self, calls: _Calls, claim: str):
        self._calls, self.claim = calls, claim

    async def get(self, key: str) -> Optional[dict]:
        """{"key", "version", "value", "writer", "updatedAt"} or None."""
        return await self._calls.call({"op": "contextGet", "claim": self.claim, "key": key})

    async def put(self, key: str, value: Any, if_version: Optional[int] = None) -> dict:
        """if_version: None = overwrite, 0 = create only, N = only if currently at N.
        Raises ClankrError(code="Conflict") when someone else wrote first."""
        op = {"op": "contextPut", "claim": self.claim, "key": key, "value": value}
        if if_version is not None:
            op["ifVersion"] = if_version
        return await self._calls.call(op)

    async def list(self, prefix: str = "") -> list:
        return await self._calls.call({"op": "contextList", "claim": self.claim, "prefix": prefix})


class Message:
    """A delivered mailbox message. Delivery is at-least-once: ack when done, nack to retry."""

    def __init__(self, calls: "_Calls", mailbox: str, raw: dict):
        self._calls, self.mailbox = calls, mailbox
        self.id = raw["id"]
        self.epoch = raw["leaseEpoch"]
        self.sender = raw["sender"]
        self.correlation_id = raw.get("correlationId")
        self.payload = raw["payload"]
        self.deliveries = raw["deliveries"]

    async def ack(self) -> None:
        await self._calls.call({"op": "mailboxAck", "mailbox": self.mailbox, "id": self.id, "epoch": self.epoch})

    async def nack(self, delay_seconds: float = 0, reason: str = "") -> None:
        """Give it back for redelivery (dead-lettered after the mailbox's maxDeliveries)."""
        await self._calls.call({"op": "mailboxNack", "mailbox": self.mailbox, "id": self.id, "epoch": self.epoch, "delaySeconds": delay_seconds, "reason": reason})


class _Mailbox:
    def __init__(self, calls: "_Calls", name: str):
        self._calls, self.name = calls, name

    async def send(self, payload: Any, correlation_id: Optional[str] = None, dedup_key: Optional[str] = None, ttl_seconds: Optional[int] = None) -> dict:
        """Returns {"message": {...}, "duplicate": bool}. Same dedup_key -> same message."""
        op = {"op": "mailboxSend", "mailbox": self.name, "payload": payload}
        for k, v in (("correlationId", correlation_id), ("dedupKey", dedup_key), ("ttlSeconds", ttl_seconds)):
            if v is not None:
                op[k] = v
        return await self._calls.call(op)

    async def receive(self, max: int = 10, visibility_seconds: int = 60, wait_seconds: int = 0) -> list:
        """Leases up to `max` messages; unacked ones come back after `visibility_seconds`."""
        raw = await self._calls.call({"op": "mailboxReceive", "mailbox": self.name, "max": max, "visibilitySeconds": visibility_seconds, "waitSeconds": wait_seconds})
        return [Message(self._calls, self.name, m) for m in raw]


class Context:
    def __init__(self, calls: _Calls, welcome: dict):
        self._calls = calls
        self.namespace = welcome["namespace"]
        self.agent = welcome["agent"]
        self.instance = welcome["instance"]
        self.models = _Models(calls)
        self.tools = _Tools(calls)

    def context(self, claim: str) -> _ContextClaim:
        return _ContextClaim(self._calls, claim)

    def mailbox(self, name: str) -> _Mailbox:
        return _Mailbox(self._calls, name)

    async def log(self, message: str, level: str = "info") -> None:
        await self._calls.conn.send({"type": "log", "level": level, "message": message})


Handler = Callable[[Task, Context], Awaitable[Any]]


class AgentApp:
    def __init__(self) -> None:
        self.handlers: dict = {}

    def task(self, node: str = "*") -> Callable[[Handler], Handler]:
        def register(fn: Handler) -> Handler:
            self.handlers[node] = fn
            return fn
        return register

    def run(self) -> None:
        asyncio.run(self._main())

    async def _main(self) -> None:
        path = os.environ.get("CLANKR_SOCKET")
        if not path:
            sys.exit("CLANKR_SOCKET is not set; agents are started by clankr-node")
        reader, writer = await asyncio.open_unix_connection(path)
        conn = _Conn(reader, writer)
        await conn.send({"type": "hello", "abiMajor": ABI_MAJOR, "abiMinor": ABI_MINOR, "sdk": f"python/{__version__}"})
        welcome = await conn.recv()
        if welcome["type"] == "reject":
            sys.exit(f"node rejected this agent: {welcome['reason']}")
        calls = _Calls(conn)
        ctx = Context(calls, welcome)
        await conn.send({"type": "ready"})

        running: set = set()
        while True:
            try:
                frame = await conn.recv()
            except asyncio.IncompleteReadError:
                return  # node went away
            kind = frame["type"]
            if kind == "deliver":
                t = asyncio.create_task(self._handle(conn, ctx, frame))
                running.add(t)
                t.add_done_callback(running.discard)
            elif kind == "reply":
                calls.resolve(frame)
            elif kind == "shutdown":
                if running:
                    await asyncio.wait(running, timeout=frame.get("graceMs", 5000) / 1000)
                return

    async def _handle(self, conn: _Conn, ctx: Context, d: dict) -> None:
        task = Task(d["runId"], d["task"], d["node"], d["attempt"], d["idempotencyKey"], d["input"])
        _current.set({"task": task, "n": 0})
        fn = self.handlers.get(task.node) or self.handlers.get("*")
        try:
            if fn is None:
                raise Permanent(f"no handler for node {task.node!r}")
            result = await fn(task, ctx)
            await conn.send({"type": "complete", "deliveryId": d["deliveryId"], "result": result})
        except Permanent as e:
            await conn.send({"type": "fail", "deliveryId": d["deliveryId"], "error": str(e), "retryable": False})
        except Exception as e:  # noqa: BLE001 - any handler error is reported, never crashes the agent
            await conn.send({"type": "fail", "deliveryId": d["deliveryId"], "error": f"{type(e).__name__}: {e}", "retryable": True})
