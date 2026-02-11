#!/usr/bin/env python3
"""Analyze decoded TachTalk capture files for response anomalies."""

import re
import sys
from pathlib import Path


def parse_captures(text):
    """Split decoded output into per-capture record lists."""
    parts = re.split(r"^=== (capture_.*?\.ttcap) ===$", text, flags=re.MULTILINE)
    captures = []
    for i in range(1, len(parts), 2):
        name = parts[i]
        content = parts[i + 1]
        records = []
        for line in content.split("\n"):
            m = re.match(
                r"\s+(\d+)\s+(\d+)\s+(TX|RX|CONNECT|DISCONNECT)\s+(\d+)\s+(.*)",
                line,
            )
            if m:
                records.append(
                    {
                        "num": int(m.group(1)),
                        "time": int(m.group(2)),
                        "type": m.group(3),
                        "bytes": int(m.group(4)),
                        "data": m.group(5).strip(),
                    }
                )
        captures.append({"name": name, "records": records})
    return captures


def analyze_capture(cap):
    """Print anomaly analysis for one capture."""
    recs = cap["records"]
    tx_recs = [r for r in recs if r["type"] == "TX"]
    rx_recs = [r for r in recs if r["type"] == "RX"]

    print(f"=== {cap['name']} ===")

    # ---- ATZ response ----
    for j, r in enumerate(recs):
        if r["type"] == "TX" and "ATZ" in r["data"]:
            for k in range(j + 1, min(j + 5, len(recs))):
                if recs[k]["type"] == "RX":
                    print(
                        f"  ATZ response: {recs[k]['data']!r}"
                        f" ({recs[k]['bytes']} bytes)"
                    )
                    break
            break

    # ---- Split vs combined prompt delivery ----
    split_prompt = sum(
        1
        for r in rx_recs
        if r["data"] in (r"\r>", r"\r\r>")
    )
    prompt_with_data = sum(
        1
        for r in rx_recs
        if ">" in r["data"] and len(r["data"]) > 5
    )
    print(f"  Split prompt (separate '\\r>' read): {split_prompt}")
    print(f"  Prompt combined with data: {prompt_with_data}")

    # ---- SEARCHING / NO DATA ----
    searching = sum(1 for r in rx_recs if "SEARCHING" in r["data"])
    no_data = sum(1 for r in rx_recs if "NO DATA" in r["data"])
    print(f"  SEARCHING... responses: {searching}")
    print(f"  NO DATA responses: {no_data}")

    # ---- Commands with >1 TCP read per response ----
    multi_rx = 0
    for j in range(len(recs)):
        if recs[j]["type"] == "TX":
            rx_count = 0
            for k in range(j + 1, len(recs)):
                if recs[k]["type"] == "TX":
                    break
                if recs[k]["type"] == "RX":
                    rx_count += 1
            if rx_count > 1:
                multi_rx += 1
    print(f"  Commands with split RX (>1 read per response): {multi_rx}")

    # ---- ATH1 PCI byte verification ----
    h1_responses = [r for r in rx_recs if "7E8" in r["data"]]
    if h1_responses:
        pci_issues = []
        for r in h1_responses:
            d = r["data"].replace(r"\r\r>", "").replace(r"\r>", "")
            if len(d) >= 5:
                pci_hex = d[3:5]
                data_after_pci = d[5:]
                try:
                    pci = int(pci_hex, 16)
                    actual_bytes = len(data_after_pci) // 2
                    if pci != actual_bytes:
                        pci_issues.append(
                            f"PCI={pci}, actual={actual_bytes}, line={d!r}"
                        )
                except ValueError:
                    pass
        print(
            f"  ATH1 responses: {len(h1_responses)},"
            f" PCI mismatches: {len(pci_issues)}"
        )
        for issue in pci_issues[:5]:
            print(f"    {issue}")

    # ---- Multi-PID response format ----
    multi_pid_rx = [
        r
        for r in rx_recs
        if "410C" in r["data"] and "05" in r["data"] and ">" in r["data"]
    ]
    if multi_pid_rx:
        print(f"  Multi-PID combined responses (010C+05): {len(multi_pid_rx)}")
        for r in multi_pid_rx[:3]:
            print(f"    Example: {r['data']!r}")

    # ---- Count suffix transition ----
    obd_tx = [r for r in tx_recs if not r["data"].startswith("AT")]
    with_count = [r for r in obd_tx if " 1\\r" in r["data"]]
    without_count = [r for r in obd_tx if " 1\\r" not in r["data"]]
    if with_count and without_count:
        print(f"  Commands without count suffix: {len(without_count)}")
        print(f"  Commands with ' 1' suffix: {len(with_count)}")

        # Find the transition point
        for j, r in enumerate(obd_tx):
            if " 1\\r" in r["data"]:
                prev = obd_tx[j - 1] if j > 0 else None
                if prev and " 1\\r" not in prev["data"]:
                    print(
                        f"  Mode transition at record #{r['num']}"
                        f" (t={r['time']}ms)"
                    )
                break

    # ---- Timing: round-trip with vs without count ----
    for label, subset in [("without count", without_count), ("with count", with_count)]:
        if not subset:
            continue
        rts = []
        for r in subset:
            idx = next(
                (i for i, rec in enumerate(recs) if rec["num"] == r["num"]), None
            )
            if idx is None:
                continue
            for k in range(idx + 1, min(idx + 5, len(recs))):
                if recs[k]["type"] == "RX":
                    rts.append(recs[k]["time"] - r["time"])
                    break
        if rts:
            avg = sum(rts) / len(rts)
            print(
                f"  RT ({label}): avg={avg:.1f}ms,"
                f" min={min(rts)}ms, max={max(rts)}ms,"
                f" n={len(rts)}"
            )

    print()


def main():
    path = Path(__file__).parent / "decoded.txt"
    if not path.exists():
        print(f"Error: {path} not found", file=sys.stderr)
        sys.exit(1)

    text = path.read_text()
    captures = parse_captures(text)
    print(f"Parsed {len(captures)} captures\n")

    for cap in captures:
        analyze_capture(cap)


if __name__ == "__main__":
    main()
