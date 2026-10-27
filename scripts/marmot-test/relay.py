"""A tiny in-memory Nostr relay (NIP-01 + replaceable/addressable kinds + ephemeral), for local tests."""
import asyncio, json, sys
import websockets

events = {}          # id -> event
subs = {}            # (ws, subid) -> filters
clients = set()

def matches(f, e):
    if "ids" in f and e["id"] not in f["ids"]: return False
    if "authors" in f and e["pubkey"] not in f["authors"]: return False
    if "kinds" in f and e["kind"] not in f["kinds"]: return False
    if "since" in f and e["created_at"] < f["since"]: return False
    if "until" in f and e["created_at"] > f["until"]: return False
    for k, v in f.items():
        if k.startswith("#") and len(k) == 2:
            vals = [t[1] for t in e["tags"] if len(t) > 1 and t[0] == k[1]]
            if not any(x in v for x in vals): return False
    return True

def store(e):
    k = e["kind"]
    if 20000 <= k < 30000: return
    if k in (0, 3) or 10000 <= k < 20000:
        for i, o in list(events.items()):
            if o["kind"] == k and o["pubkey"] == e["pubkey"] and o["created_at"] <= e["created_at"]: del events[i]
    elif 30000 <= k < 40000:
        d = next((t[1] for t in e["tags"] if t and t[0] == "d"), "")
        for i, o in list(events.items()):
            od = next((t[1] for t in o["tags"] if t and t[0] == "d"), "")
            if o["kind"] == k and o["pubkey"] == e["pubkey"] and od == d and o["created_at"] <= e["created_at"]: del events[i]
    events[e["id"]] = e

async def handler(ws):
    clients.add(ws)
    try:
        async for raw in ws:
            try: msg = json.loads(raw)
            except Exception: continue
            t = msg[0]
            if t == "EVENT":
                e = msg[1]; store(e)
                await ws.send(json.dumps(["OK", e["id"], True, ""]))
                for (c, sid), fs in list(subs.items()):
                    if any(matches(f, e) for f in fs):
                        try: await c.send(json.dumps(["EVENT", sid, e]))
                        except Exception: pass
            elif t == "REQ":
                sid, fs = msg[1], msg[2:]
                subs[(ws, sid)] = fs
                out = []
                for f in fs:
                    m = sorted([e for e in events.values() if matches(f, e)], key=lambda e: -e["created_at"])
                    if "limit" in f: m = m[: f["limit"]]
                    out += m
                seen = set()
                for e in out:
                    if e["id"] in seen: continue
                    seen.add(e["id"]); await ws.send(json.dumps(["EVENT", sid, e]))
                await ws.send(json.dumps(["EOSE", sid]))
            elif t == "CLOSE":
                subs.pop((ws, msg[1]), None)
            elif t == "AUTH":
                await ws.send(json.dumps(["OK", msg[1].get("id", ""), True, ""]))
    finally:
        clients.discard(ws)
        for k in [k for k in subs if k[0] is ws]: subs.pop(k, None)

async def main(port):
    async with websockets.serve(handler, "127.0.0.1", port, max_size=4 << 20):
        print("relay on", port, flush=True)
        await asyncio.Future()

asyncio.run(main(int(sys.argv[1])))
