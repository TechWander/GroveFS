#!/usr/bin/env python3
"""Zero-dependency stdio JSON-RPC client for `grovefs mcp`.

Speaks exactly the framing the server uses: newline-delimited JSON-RPC 2.0 over
the child's stdin/stdout (protocol 2025-06-18). It handshakes, optionally
presents an apiKey, sends ONE request, prints the answer, and shuts the server
down. Useful whenever the agent's runtime does not manage MCP servers itself
(shell work, CI, smoke tests) — the frames are the same ones any MCP client sends.

Usage:
  mcp_call.py [--bin PATH] [--meta-dir D] [--api-key ID:SECRET]
              [--server-arg FLAG]... [--timeout SECS] [--json] [--quiet] CMD

Commands:
  list                        tools/list   (prints one tool per line; --json for schema)
  call TOOL [JSON_ARGS]       tools/call
  resources                   resources/list
  templates                   resources/templates/list
  read URI                    resources/read
  guide                       resources/read grovefs://guide (the operating manual)
  views                       resources/read grovefs://views (whole-store snapshot)
  tree NAME [PATH] [DEPTH] [MAX]
                              resources/read grovefs://view/NAME/tree?path=…
  prompts                     prompts/list
  prompt NAME [JSON_ARGS]     prompts/get
  complete NAME PREFIX        completion/complete (view-name completion)
  setlevel LEVEL [CMD ...]    logging/setLevel, then run another command and show pushes
  ping                        ping
  raw JSON_FRAME              send any frame verbatim

Exit codes: 0 success · 2 JSON-RPC error frame (-32001 auth, -32002 bad resource
URI, -32601 unknown method, -32602 bad params, -32700 parse error) · 3 the tool
refused (MCP reports governance failures inside the result as isError) · 4 the
server could not be started or answered nothing.

Notes that match the server: frames with no id (notifications) are never
answered, so don't wait for one; server-pushed notifications/message frames go to
stderr so stdout stays a single parseable payload.
"""

import json
import os
import select
import shutil
import subprocess
import sys
import urllib.parse

PROTOCOL_VERSION = "2025-06-18"


def die(msg, code):
    sys.stderr.write("mcp_call: %s\n" % msg)
    sys.exit(code)


def find_binary(explicit):
    # The skill lives at <repo>/skills/grovefs-mcp/scripts/, so the release
    # binary is two levels above the script's directory; the cwd-relative
    # candidates cover invocations from the repo root or an install prefix.
    here = os.path.dirname(os.path.abspath(__file__))
    for cand in (
        explicit,
        os.environ.get("GROVEFS_BIN"),
        shutil.which("grovefs"),
        os.path.join(here, "..", "..", "target", "release", "grovefs"),
        "./target/release/grovefs",
        "../release/grovefs",
    ):
        if cand and os.path.isfile(cand) and os.access(cand, os.X_OK):
            return cand
    die("no grovefs binary found (pass --bin, or export GROVEFS_BIN)", 4)


class Mcp:
    def __init__(self, binpath, server_args, timeout):
        argv = [binpath, "mcp"] + list(server_args)
        self.p = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            bufsize=0,
        )
        self.timeout = timeout
        self.next_id = 1

    def send(self, frame):
        data = (json.dumps(frame) + "\n").encode()
        try:
            self.p.stdin.write(data)
            self.p.stdin.flush()
        except (BrokenPipeError, ValueError):
            die("the server closed our stdin (is `%s mcp` working?)" % " ".join(self.p.args), 4)

    def recv_id(self, want):
        """Return the frame whose id == want, printing any interleaved push."""
        deadline_left = self.timeout
        while True:
            r, _, _ = select.select([self.p.stdout], [], [], deadline_left)
            if not r:
                die("no answer within %ss (a frame with no id is never answered)" % self.timeout, 4)
            line = self.p.stdout.readline()
            if not line:
                die("server exited (stderr hidden; run with --server-arg to see the command)", 4)
            line = line.strip()
            if not line:
                continue
            try:
                frame = json.loads(line)
            except ValueError:
                die("non-JSON line from stdout: %r" % line[:200], 4)
            if "id" not in frame and "method" in frame:
                self.push(frame)
                continue
            if frame.get("id") == want:
                # The server writes its notification frames AFTER the response,
                # so take a short breath for whatever it already queued.
                self.drain(0.4)
                return frame
            sys.stderr.write("[skip] unexpected id %s\n" % frame.get("id"))

    def push(self, frame):
        sys.stderr.write("[push] %s %s\n" % (frame["method"], json.dumps(frame.get("params"))))

    def drain(self, seconds):
        """Read any push frames already in flight (never waits for an answer)."""
        left = seconds
        while left > 0:
            r, _, _ = select.select([self.p.stdout], [], [], left)
            if not r:
                return
            line = self.p.stdout.readline().strip()
            if not line:
                return
            left -= 0.05
            try:
                frame = json.loads(line)
            except ValueError:
                continue
            if "id" not in frame and "method" in frame:
                self.push(frame)

    def request(self, method, params=None):
        rid = self.next_id
        self.next_id += 1
        frame = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            frame["params"] = params
        self.send(frame)
        return self.recv_id(rid)

    def close(self):
        try:
            self.p.stdin.close()
        except OSError:
            pass
        self.p.terminate()


def handshake(mcp, api_key, quiet):
    params = {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "grovefs-mcp-call", "version": "1"},
    }
    if api_key:
        params["apiKey"] = api_key
    frame = mcp.request("initialize", params)
    if "error" in frame:
        show_error(frame["error"])
        sys.exit(2)
    info = frame["result"].get("serverInfo", {})
    if not quiet:
        sys.stderr.write(
            "[handshake] %s %s, protocol %s%s\n"
            % (
                info.get("name", "?"),
                info.get("version", "?"),
                frame["result"].get("protocolVersion", "?"),
                ", authenticated" if api_key else "",
            )
        )
    # `initialized` is a notification: the server answers nothing on purpose.
    mcp.send({"jsonrpc": "2.0", "method": "notifications/initialized"})


def show_error(err):
    sys.stderr.write("JSON-RPC error %s: %s\n" % (err.get("code"), err.get("message")))
    if err.get("code") == -32001:
        sys.stderr.write(
            "  → this entrance is gated: present apiKey in initialize params "
            "(--api-key ID:SECRET) or export GROVEFS_MCP_API_KEY for the server\n"
        )


def show_result(method, result, as_json):
    if as_json:
        print(json.dumps(result, indent=2, ensure_ascii=False))
        return 0
    if method == "tools/list":
        for t in result.get("tools", []):
            desc = (t.get("description") or "").split(". ")[0][:96]
            req = t.get("inputSchema", {}).get("required", [])
            # The safety hint decides whether a client may auto-approve the call.
            mode = "ro" if t.get("annotations", {}).get("readOnlyHint") else "rw"
            print("[%s] %-22s required=[%s]  %s" % (mode, t["name"], ",".join(req), desc))
        print("(%d tools)" % len(result.get("tools", [])))
        return 0
    if method == "prompts/list":
        for pr in result.get("prompts", []):
            args = ",".join(a["name"] for a in pr.get("arguments", []))
            print("%-16s args=[%s]  %s" % (pr["name"], args, pr.get("description", "")))
        return 0
    if method in ("resources/list", "resources/templates/list"):
        for r in result.get("resources", result.get("resourceTemplates", [])):
            print("%s  (%s)" % (r.get("uri", r.get("uriTemplate")), r.get("name", "")))
        return 0
    if method == "prompts/get":
        for m in result.get("messages", []):
            print(m.get("content", {}).get("text", ""))
        return 0
    if method == "resources/read":
        # Markdown (the guide) reads as-is; everything else is JSON in `text`.
        for c in result.get("contents", []):
            body = c.get("text", "")
            if str(c.get("mimeType", "")).startswith("text/markdown"):
                print(body)
            else:
                try:
                    print(json.dumps(json.loads(body), indent=2, ensure_ascii=False))
                except ValueError:
                    print(body)
        return 0
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


def show_tool_result(frame, as_json):
    """tools/call: governance refusals arrive INSIDE the result as isError."""
    if "error" in frame:
        show_error(frame["error"])
        return 2
    res = frame.get("result", {})
    texts = [c.get("text", "") for c in res.get("content", []) if c.get("type") == "text"]
    if as_json:
        print(json.dumps(res, indent=2, ensure_ascii=False))
    else:
        print("\n".join(texts).rstrip())
    if res.get("isError"):
        sys.stderr.write("the tool refused this call (isError) — not a protocol error\n")
        return 3
    return 0


def parse_args():
    a = sys.argv[1:]
    opt = {
        "bin": None,
        "db_path": None,
        "api_key": os.environ.get("GROVEFS_MCP_API_KEY_PRESENT"),
        "server_args": [],
        "timeout": 15.0,
        "json": False,
        "quiet": False,
    }
    i = 0
    while i < len(a):
        t = a[i]
        if t == "--bin":
            opt["bin"] = a[i + 1]
            i += 2
        elif t == "--meta-dir":
            opt["db_path"] = a[i + 1]
            i += 2
        elif t == "--api-key":
            opt["api_key"] = a[i + 1]
            i += 2
        elif t == "--server-arg":
            opt["server_args"].append(a[i + 1])
            i += 2
        elif t == "--timeout":
            opt["timeout"] = float(a[i + 1])
            i += 2
        elif t == "--json":
            opt["json"] = True
            i += 1
        elif t == "--quiet":
            opt["quiet"] = True
            i += 1
        elif t in ("-h", "--help"):
            print(__doc__)
            sys.exit(0)
        else:
            return opt, a[i:]
    return opt, []


def jload(text, what):
    try:
        return json.loads(text) if text else {}
    except ValueError:
        die("%s is not valid JSON: %r" % (what, text[:120]), 4)


def main():
    opt, rest = parse_args()
    if not rest:
        print(__doc__)
        sys.exit(4)

    server_args = list(opt["server_args"])
    if opt["db_path"]:
        server_args += ["--meta-dir", opt["db_path"]]

    mcp = Mcp(find_binary(opt["bin"]), server_args, opt["timeout"])
    try:
        handshake(mcp, opt["api_key"], opt["quiet"])
        cmd = rest[0]

        if cmd == "raw":
            frame = jload(rest[1] if len(rest) > 1 else "{}", "raw frame")
            mcp.send(frame)
            rid = frame.get("id")
            got = mcp.recv_id(rid) if rid is not None else None
            print(json.dumps(got, indent=2, ensure_ascii=False) if got else "(no answer — notification frames are never answered)")
            return 0

        if cmd == "list":
            method, params, rc = "tools/list", None, "plain"
        elif cmd == "resources":
            method, params, rc = "resources/list", None, "plain"
        elif cmd == "templates":
            method, params, rc = "resources/templates/list", None, "plain"
        elif cmd == "prompts":
            method, params, rc = "prompts/list", None, "plain"
        elif cmd == "ping":
            method, params, rc = "ping", None, "plain"
        elif cmd == "read":
            method, params, rc = "resources/read", {"uri": rest[1]}, "plain"
        elif cmd == "guide":
            method, params, rc = "resources/read", {"uri": "grovefs://guide"}, "plain"
        elif cmd == "views":
            method, params, rc = "resources/read", {"uri": "grovefs://views"}, "plain"
        elif cmd == "tree":
            if len(rest) < 2:
                die("`tree` needs a view name", 4)
            uri = "grovefs://view/%s/tree" % urllib.parse.quote(rest[1], safe="")
            q = []
            if len(rest) > 2 and rest[2]:
                q.append("path=" + urllib.parse.quote(rest[2], safe=""))
            if len(rest) > 3 and rest[3]:
                q.append("depth=" + rest[3])
            if len(rest) > 4 and rest[4]:
                q.append("max=" + rest[4])
            if q:
                uri += "?" + "&".join(q)
            method, params, rc = "resources/read", {"uri": uri}, "plain"
        elif cmd == "prompt":
            method, params, rc = "prompts/get", {"name": rest[1], "arguments": jload(rest[2] if len(rest) > 2 else "{}", "prompt arguments")}, "plain"
        elif cmd == "complete":
            method, params, rc = (
                "completion/complete",
                {"ref": {"type": "ref/prompt", "name": "reshape-view"},
                 "argument": {"name": rest[1], "value": rest[2] if len(rest) > 2 else ""}},
                "plain",
            )
        elif cmd == "setlevel":
            mcp.request("logging/setLevel", {"level": rest[1]})
            sys.stderr.write("[setlevel] %s — later calls may push notifications/message\n" % rest[1])
            rest = ["call", "list_views", "{}"] if len(rest) == 2 else rest[2:]
            if rest[0] != "call":
                return 0
            method, params, rc = "tools/call", {"name": rest[1], "arguments": jload(rest[2] if len(rest) > 2 else "{}", "tool arguments")}, "tool"
        elif cmd == "call":
            if len(rest) < 2:
                die("`call` needs a tool name", 4)
            method = "tools/call"
            params = {"name": rest[1], "arguments": jload(rest[2] if len(rest) > 2 else "{}", "tool arguments")}
            rc = "tool"
        else:
            die("unknown command %r (see --help)" % cmd, 4)

        frame = mcp.request(method, params)
        if rc == "tool":
            return show_tool_result(frame, opt["json"])
        if "error" in frame:
            show_error(frame["error"])
            return 2
        return show_result(method, frame.get("result", {}), opt["json"])
    finally:
        mcp.close()


if __name__ == "__main__":
    sys.exit(main())
