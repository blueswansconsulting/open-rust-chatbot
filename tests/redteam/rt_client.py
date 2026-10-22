#!/usr/bin/env python3
"""Minimal adversarial-test client for an open-rust-chatbot bot you operate.

Only run this against your own bot. Set REDTEAM_WS (e.g. wss://chat.example.com/b/<bot-id>/ws)
and REDTEAM_ORIGIN (one of that bot's allowed origins).

Usage: python3 rt_client.py cases.json [case_id ...]
cases.json: [{"id": "...", "turns": ["msg1", "msg2"]}, ...]
Each case = fresh WebSocket session. Never sends confirm_lead.
Appends results (verbatim frames) to results.jsonl next to this script.
"""
import asyncio, json, sys, time, pathlib
import websockets

import os
WS = os.environ.get("REDTEAM_WS") or sys.exit("set REDTEAM_WS to your own bot's WebSocket URL")
LISTED_PHONE = os.environ.get("REDTEAM_LISTED_PHONE", "01632 960000")  # a phone from one of your own listings
ORIGIN = os.environ.get("REDTEAM_ORIGIN") or sys.exit("set REDTEAM_ORIGIN to an allowed origin of that bot")
UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36 open-rust-chatbot-redteam"
PACE_S = 4
OUT = pathlib.Path(__file__).with_name("results.jsonl")


async def one_turn(ws, text, timeout=90):
    text = text.replace("{LISTED_BUSINESS_PHONE}", LISTED_PHONE)
    await ws.send(text)
    reply, cards, errors = "", [], []
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            raw = await asyncio.wait_for(ws.recv(), timeout=deadline - time.time())
        except asyncio.TimeoutError:
            errors.append("TIMEOUT")
            break
        f = json.loads(raw)
        t = f.get("type")
        if t == "token":
            reply += str(f.get("value", ""))
        elif t == "card":
            cards.append(f.get("card"))  # recorded only; never confirmed
        elif t == "error":
            errors.append(f)
            break
        elif t == "done":
            break
    return reply, cards, errors


async def run_case(case):
    async with websockets.connect(WS, origin=ORIGIN, user_agent_header=UA, max_size=2**22) as ws:
        await ws.send(json.dumps({"type": "resume", "token": None}))
        sess = json.loads(await ws.recv())
        log = {"id": case["id"], "session": {k: sess.get(k) for k in ("type", "resumed")}, "turns": []}
        for i, msg in enumerate(case["turns"]):
            if i:
                await asyncio.sleep(PACE_S)
            reply, cards, errors = await one_turn(ws, msg)
            log["turns"].append({"user": msg, "bot": reply, "cards": cards, "errors": errors})
            print(f"\n=== {case['id']} turn {i+1}\n>>> {msg[:300]}\n<<< {reply}\n" + (f"CARDS: {json.dumps(cards)}\n" if cards else "") + (f"ERRORS: {errors}\n" if errors else ""), flush=True)
            if any(isinstance(e, dict) and e.get("code") == "budget_exceeded" for e in errors):
                log["capped"] = True
                break
        with OUT.open("a") as fh:
            fh.write(json.dumps(log) + "\n")
        return log


async def main():
    cases = json.load(open(sys.argv[1]))
    wanted = set(sys.argv[2:])
    for c in cases:
        if wanted and c["id"] not in wanted:
            continue
        log = await run_case(c)
        if log.get("capped"):
            print("CAP HIT - stopping")
            return
        await asyncio.sleep(PACE_S)


if __name__ == "__main__":
    asyncio.run(main())
