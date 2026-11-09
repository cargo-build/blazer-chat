#!/usr/bin/env python3
"""Frame-time bench over the control socket (m10 DoD 20/100/400).

Opens rooms and reads the frame-interval telemetry from /state.
NO messages are sent — /open alone drives real frames, so the
live-send policy (self-DM only) stays intact.

WARNING: /open switches the VISIBLE room of the running app — run
this only while nobody is using the window (--yes confirms).

Usage:
  scripts/bench_perf.py --rooms <id[,id...]> --yes   # explicit rooms
  scripts/bench_perf.py --scan --yes                 # probe rooms until
                                                     # 20/100/400 buckets fill
Buckets pass when avg < 16.6 ms (60 FPS). Off-view/occluded
windows produce no frames and no samples — keep the tile in view.
"""
import argparse
import json
import socket
import sys
import time
import os


def call(sock_path: str, method: str, path: str, body=None):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(10)
        s.connect(sock_path)
        payload = body if body is not None else {}
        head = f"{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {len(json.dumps(payload))}\r\n\r\n"
        s.sendall(head.encode() + json.dumps(payload).encode())
        raw = b""
        while chunk := s.recv(65536):
            raw += chunk
    return json.loads(raw.split(b"\r\n\r\n", 1)[1])


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", default=os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/run/user/0"), "blazer-chat.sock"))
    ap.add_argument("--rooms", help="comma-separated room ids")
    ap.add_argument("--scan", action="store_true", help="probe listed rooms until buckets fill")
    ap.add_argument("--scan-limit", type=int, default=30)
    ap.add_argument("--budget-ms", type=float, default=16.6)
    ap.add_argument("--yes", action="store_true", help="confirm the visible room will move")
    args = ap.parse_args()

    if not args.yes:
        print("refusing without --yes: /open moves the visible room", file=sys.stderr)
        return 2
    if not args.rooms and not args.scan:
        print("need --rooms or --scan", file=sys.stderr)
        return 2

    state = call(args.socket, "GET", "/state")
    room_ids = args.rooms.split(",") if args.rooms else [
        r["id"] for r in state.get("rooms", [])[: args.scan_limit]
    ]

    buckets = {}  # bucket target -> (room_id, avg, max, samples)
    for room_id in room_ids:
        call(args.socket, "POST", "/open", {"room_id": room_id})
        time.sleep(0.6)  # let history land and frames render
        snap = call(args.socket, "GET", "/state")
        rows = snap.get("active_room_messages", 0)
        avg, mx = snap.get("frame_ms_avg"), snap.get("frame_ms_max")
        if avg is None:
            print(f"{room_id}: {rows} rows — no frame samples (off-view throttle?)")
            continue
        print(f"{room_id}: {rows} rows — avg {avg:.1f} ms, max {mx:.1f} ms")
        for target in (20, 100, 400):
            if target not in buckets and rows >= target:
                buckets[target] = (room_id, avg, mx)
                break

    ok = True
    print("\n== buckets (the largest room at or above each target) ==")
    for target in (20, 100, 400):
        found = buckets.get(target)
        if found is None:
            print(f"{target}+ rows: no candidate room probed")
            continue
        room_id, avg, mx = found
        verdict = "PASS" if avg < args.budget_ms else "FAIL"
        ok &= avg < args.budget_ms
        print(f"{target}+ rows ({room_id}): avg {avg:.1f} ms < {args.budget_ms} -> {verdict}")
    return 0 if ok and buckets else 1


if __name__ == "__main__":
    sys.exit(main())
