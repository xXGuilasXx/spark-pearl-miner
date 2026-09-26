#!/usr/bin/env python3
"""spm-probe — authorize-only Pearl stratum dialect probe (stdlib only).

Opens ONE connection per hypothesis, sends the handshake, listens for jobs for a while and
records everything as redacted JSONL. It NEVER sends mining.submit. Rate limit: at most
MAX_CONN connections per pool per run (default 5) — respect pools.

Hypotheses (tried in order, stop at the first `result:true`):
  H1  object authorize-first      {"id":1,"method":"mining.authorize","params":{"wallet":W,"worker":N,"agent":A}}
  H1b object, combined login       {"id":1,"method":"mining.authorize","params":{"wallet":"W.N","agent":A}}
  H2  array (stratum v1)           subscribe [A] then authorize ["W.N","x"]
  H3  CryptoNote login             {"id":1,"method":"login","params":{"login":W,"pass":"x","agent":A}}

Usage:
  spm-probe.py --host br.pearl.herominers.com --port 1200 --wallet prl1... [--worker spm-probe]
               [--tls auto|on|off] [--listen 600] [--out tests/fixtures/capture-herominers-br.jsonl]
"""
import argparse, json, socket, ssl, sys, time, hashlib

AGENT = "spark-pearl-miner-probe/0.0.1"
MAX_CONN = 5

def redact(obj, wallet):
    s = json.dumps(obj)
    if wallet:
        s = s.replace(wallet, "<WALLET>")
    return json.loads(s)

def connect(host, port, tls_mode, timeout=15, insecure=False):
    raw = socket.create_connection((host, port), timeout=timeout)
    if tls_mode == "off":
        return raw, "plain"
    ctx = ssl.create_default_context()
    if insecure:
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
    try:
        s = ctx.wrap_socket(raw, server_hostname=host)
        return s, "tls" + (" (unverified)" if insecure else "")
    except ssl.SSLError as e:
        raw.close()
        if tls_mode == "on":
            raise
        raw = socket.create_connection((host, port), timeout=timeout)
        return raw, f"plain (tls failed: {e.__class__.__name__})"

def send(sock, obj):
    sock.sendall((json.dumps(obj) + "\n").encode())

def recv_lines(sock, deadline, maxbytes=4 * 1024 * 1024):
    buf = b""
    sock.settimeout(5)
    while time.time() < deadline:
        try:
            chunk = sock.recv(65536)
        except socket.timeout:
            continue
        if not chunk:
            yield None  # EOF
            return
        buf += chunk
        if len(buf) > maxbytes:
            yield {"_error": "line too long"}
            return
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            line = line.strip()
            if not line:
                continue
            try:
                yield json.loads(line)
            except json.JSONDecodeError:
                yield {"_raw": line[:200].decode(errors="replace")}

def hypotheses(wallet, worker):
    return [
        ("H1", [{"id": 1, "method": "mining.authorize", "params": {"wallet": wallet, "worker": worker, "agent": AGENT}}]),
        ("H1b", [{"id": 1, "method": "mining.authorize", "params": {"wallet": f"{wallet}.{worker}", "agent": AGENT}}]),
        ("H2", [{"id": 1, "method": "mining.subscribe", "params": [AGENT]},
                {"id": 2, "method": "mining.authorize", "params": [f"{wallet}.{worker}", "x"]}]),
        ("H3", [{"id": 1, "method": "login", "params": {"login": wallet, "pass": "x", "agent": AGENT}}]),
    ]

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True); ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--wallet", required=True); ap.add_argument("--worker", default="spm-probe")
    ap.add_argument("--tls", default="auto", choices=["auto", "on", "off"])
    ap.add_argument("--listen", type=int, default=600, help="seconds to listen for jobs after a successful handshake")
    ap.add_argument("--only", default="", help="comma-separated hypothesis ids to try (default: all in order)")
    ap.add_argument("--out", default="")
    ap.add_argument("--insecure", action="store_true", help="TLS without certificate verification (probe only)")
    ap.add_argument("--jsonrpc", action="store_true", help='add "jsonrpc":"2.0" to every request')
    a = ap.parse_args()
    out = open(a.out, "a") if a.out else None
    def log(ev):
        ev["t"] = round(time.time(), 3)
        ev = redact(ev, a.wallet)
        print(json.dumps(ev), flush=True)
        if out:
            out.write(json.dumps(ev) + "\n"); out.flush()
    todo = hypotheses(a.wallet, a.worker)
    if a.only:
        keep = set(a.only.split(","))
        todo = [h for h in todo if h[0] in keep]
    conns = 0
    for hid, msgs in todo:
        if conns >= MAX_CONN:
            log({"ev": "rate-limit", "msg": f"stop: {MAX_CONN} connections used"}); break
        conns += 1
        try:
            sock, mode = connect(a.host, a.port, a.tls, insecure=a.insecure)
        except Exception as e:
            log({"ev": "connect-error", "hyp": hid, "err": repr(e)}); continue
        log({"ev": "connected", "hyp": hid, "transport": mode, "peer": f"{a.host}:{a.port}"})
        ok = False
        try:
            for m in msgs:
                if a.jsonrpc: m = {"jsonrpc": "2.0", **m}
                send(sock, m); log({"ev": "send", "hyp": hid, "msg": m})
                deadline = time.time() + 15
                for r in recv_lines(sock, deadline):
                    if r is None:
                        log({"ev": "eof", "hyp": hid}); break
                    log({"ev": "recv", "hyp": hid, "msg": r})
                    if isinstance(r, dict) and r.get("id") == m["id"]:
                        res = r.get("result")
                        if r.get("error") in (None, "", {}) and res not in (None, False):
                            ok = True
                        break
                if not ok and m is msgs[-1]:
                    break
            if ok:
                log({"ev": "handshake-ok", "hyp": hid, "transport": mode})
                deadline = time.time() + a.listen
                njobs = 0
                for r in recv_lines(sock, deadline):
                    if r is None:
                        log({"ev": "eof", "hyp": hid}); break
                    if isinstance(r, dict) and r.get("method") in ("mining.notify", "job"):
                        njobs += 1
                        p = r.get("params")
                        digest = hashlib.sha256(json.dumps(p, sort_keys=True).encode()).hexdigest()[:16]
                        log({"ev": "job", "hyp": hid, "n": njobs, "keys": sorted(p.keys()) if isinstance(p, dict) else "array",
                             "digest": digest, "msg": r})
                    else:
                        log({"ev": "recv", "hyp": hid, "msg": r})
                log({"ev": "done", "hyp": hid, "jobs": njobs})
                break
        finally:
            try: sock.close()
            except Exception: pass
        time.sleep(3)
    if out: out.close()

if __name__ == "__main__":
    main()
