#!/usr/bin/env python3
"""spm-proxy — logging TCP/TLS forwarder for Pearl stratum bring-up (stdlib only).

Listens on 127.0.0.1:PORT (plain), connects upstream (TLS or plain) and records both
directions with timestamps as redacted JSONL: the wallet is replaced by <WALLET> and any
`plain_proof*` field is replaced by {sha256, length}. Use it to capture a real miner's
exchange (e.g. CPPminer on the CPU) without ever storing a proof.

Usage: spm-proxy.py --listen 3390 --upstream br.pearl.herominers.com:1200 --tls auto --wallet prl1... --out tests/fixtures/capture-x.jsonl
"""
import argparse, asyncio, json, ssl, time, hashlib, re

async def pipe(reader, writer, direction, log, wallet):
    buf = b""
    while True:
        data = await reader.read(65536)
        if not data:
            break
        writer.write(data); await writer.drain()
        buf += data
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            log(direction, line)
    writer.close()

def make_logger(out, wallet):
    f = open(out, "a") if out else None
    def log(direction, line):
        s = line.decode(errors="replace").strip()
        if not s:
            return
        try:
            obj = json.loads(s)
            def scrub(o):
                if isinstance(o, dict):
                    return {k: ({"sha256": hashlib.sha256(str(v).encode()).hexdigest(), "len": len(str(v))}
                                if k.startswith("plain_proof") else scrub(v)) for k, v in o.items()}
                if isinstance(o, list):
                    return [scrub(x) for x in o]
                if isinstance(o, str) and wallet and wallet in o:
                    return o.replace(wallet, "<WALLET>")
                return o
            obj = scrub(obj)
        except json.JSONDecodeError:
            obj = {"_raw": (s[:200].replace(wallet, "<WALLET>") if wallet else s[:200])}
        ev = {"t": round(time.time(), 3), "dir": direction, "msg": obj}
        print(json.dumps(ev), flush=True)
        if f:
            f.write(json.dumps(ev) + "\n"); f.flush()
    return log

async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, required=True)
    ap.add_argument("--upstream", required=True, help="host:port")
    ap.add_argument("--tls", default="auto", choices=["auto", "on", "off"])
    ap.add_argument("--wallet", default="")
    ap.add_argument("--out", default="")
    a = ap.parse_args()
    host, port = a.upstream.rsplit(":", 1); port = int(port)
    log = make_logger(a.out, a.wallet)
    async def handle(cr, cw):
        ur = uw = None
        if a.tls != "off":
            try:
                ctx = ssl.create_default_context()
                ur, uw = await asyncio.open_connection(host, port, ssl=ctx, server_hostname=host)
                log("meta", b'{"upstream":"tls"}')
            except ssl.SSLError as e:
                if a.tls == "on":
                    raise
                log("meta", json.dumps({"upstream": "plain", "tls_error": e.__class__.__name__}).encode())
        if ur is None:
            ur, uw = await asyncio.open_connection(host, port)
            if a.tls == "off":
                log("meta", b'{"upstream":"plain"}')
        await asyncio.gather(pipe(cr, uw, "c->s", log, a.wallet), pipe(ur, cw, "s->c", log, a.wallet))
    server = await asyncio.start_server(handle, "127.0.0.1", a.listen)
    print(f"spm-proxy listening on 127.0.0.1:{a.listen} -> {a.upstream} (tls={a.tls})", flush=True)
    async with server:
        await server.serve_forever()

if __name__ == "__main__":
    asyncio.run(main())
