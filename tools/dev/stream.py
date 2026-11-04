#!/usr/bin/env python3
"""Finite, repeatable synthetic terminal output.

Only the pacing depends on wall time. Bytes are deterministic for identical args.
Output contains no user data and never executes a command or opens a URL.
"""

import argparse
import random
import sys
import time


def nonnegative(value: str) -> int:
    result = int(value)
    if result < 0:
        raise argparse.ArgumentTypeError("must be nonnegative")
    return result


def positive(value: str) -> int:
    result = nonnegative(value)
    if result == 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lines", type=positive, default=1000)
    parser.add_argument("--rate", type=nonnegative, default=100,
                        help="records per second; 0 sends as fast as stdout accepts")
    parser.add_argument("--payload-bytes", type=nonnegative, default=80,
                        help="ASCII payload length, excluding the fixed record prefix")
    parser.add_argument("--long-every", type=nonnegative, default=0,
                        help="use long payload every N records; 0 disables")
    parser.add_argument("--long-bytes", type=positive, default=32768)
    parser.add_argument("--chunk-bytes", type=nonnegative, default=0,
                        help="split writes into N bytes; PTY reads may coalesce them")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--mode", choices=("mixed", "progress"), default="mixed")
    args = parser.parse_args()
    if max(args.payload_bytes, args.long_bytes) > 1024 * 1024:
        parser.error("payload sizes are limited to 1 MiB")

    rng = random.Random(args.seed)
    levels = ("TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL")
    colors = ("90", "36", "32", "33", "31", "35")
    started = time.monotonic()
    try:
        for index in range(args.lines):
            if args.rate:
                remaining = started + index / args.rate - time.monotonic()
                if remaining > 0:
                    time.sleep(remaining)
            if args.mode == "progress":
                # Deliberate carriage-return overwrite, not an appended log line.
                record = f"\rprogress {index + 1:08d}/{args.lines:08d}\x1b[K"
                if index + 1 == args.lines:
                    record += "\r\n"
            else:
                level = rng.randrange(len(levels))
                count = (args.long_bytes if args.long_every and
                         (index + 1) % args.long_every == 0 else args.payload_bytes)
                payload = chr(ord("a") + index % 26) * count
                # One third of records have an explicit ANSI foreground color.
                prefix = f"\x1b[{colors[level]}m" if index % 3 == 0 else ""
                reset = "\x1b[0m" if prefix else ""
                record = (f"{prefix}2026-10-28T00:00:00Z {levels[level]} "
                          f"seq={index:08d} 中 e\u0301 "
                          f"192.0.2.1 https://example.invalid/log /tmp/synthetic.log "
                          f"{payload}{reset}\r\n")
            data = record.encode("utf-8")
            chunk = args.chunk_bytes or len(data)
            for offset in range(0, len(data), chunk):
                sys.stdout.buffer.write(data[offset:offset + chunk])
                sys.stdout.buffer.flush()
    except (BrokenPipeError, KeyboardInterrupt):
        # Normal when a test pane is closed; prevent a second flush at shutdown.
        import os
        os._exit(0)


if __name__ == "__main__":
    main()
