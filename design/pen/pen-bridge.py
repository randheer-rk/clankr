#!/usr/bin/env python3
"""Tiny stdio MCP client for the Pen desktop app: pen.py <tool> '<json args>'"""
import json, subprocess, sys, time
tool, args = sys.argv[1], json.loads(sys.argv[2] if len(sys.argv) > 2 else "{}")
p = subprocess.Popen(["/home/randheer/.local/opt/pen-dev/current/resources/app.asar.unpacked/out/mcp-server-linux-x64","--app","desktop","--agent","claudeCodeCLI"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
def send(o): p.stdin.write(json.dumps(o)+"\n"); p.stdin.flush()
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"claude-bridge","version":"0"}}})
send({"jsonrpc":"2.0","method":"notifications/initialized"})
send({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool,"arguments":args}})
t=time.time()
while time.time()-t<120:
    line=p.stdout.readline()
    if not line: break
    try: o=json.loads(line)
    except: continue
    if o.get("id")==2:
        r=o.get("result") or o.get("error")
        for c in (r.get("content") or []) if isinstance(r,dict) else []:
            if c.get("type")=="text": print(c["text"])
            elif c.get("type")=="image":
                import base64, os
                n=len([f for f in os.listdir(".") if f.startswith("pen-shot-")])
                path=os.path.abspath(f"pen-shot-{n}.png"); open(path,"wb").write(base64.b64decode(c["data"])); print("IMAGE", path)
            else: print(json.dumps(c)[:400])
        if not isinstance(r,dict) or "content" not in r: print(json.dumps(r)[:4000])
        break
p.kill()
