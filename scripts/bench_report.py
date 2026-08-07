#!/usr/bin/env python3
"""压测报告汇总：解析 bench.sh 采样的 CSV，输出 report.json + Markdown 摘要。"""
import csv
import glob
import json
import os
import platform
import re
import subprocess
import sys
from datetime import datetime, timezone


def avg_max(vals):
    vals = [v for v in vals if v is not None]
    if not vals:
        return 0.0, 0.0
    return round(sum(vals) / len(vals), 2), round(max(vals), 2)


def machine_info():
    info = {
        "machine": platform.machine(),
        "cpu_count": os.cpu_count() or 0,
        "system": platform.system(),
    }
    if platform.system() == "Darwin":
        try:
            info["product"] = (
                subprocess.run(
                    ["sw_vers", "-productVersion"], capture_output=True, text=True, timeout=5
                ).stdout.strip()
            )
        except Exception:
            pass
        try:
            out = subprocess.run(
                ["sysctl", "-n", "vm.loadavg"], capture_output=True, text=True, timeout=5
            ).stdout
            info["loadavg"] = out.strip()
        except Exception:
            pass
    return info


def main():
    report_dir = sys.argv[1]
    rooms, pairs, seconds, w, h, fps, bitrate = map(int, sys.argv[2:9])

    # metrics.csv: ts,rx_bytes,tx_bytes,rx_packets,tx_packets,clients
    rows = []
    with open(os.path.join(report_dir, "metrics.csv")) as f:
        for r in csv.DictReader(f):
            rows.append(r)
    bw_samples = []
    rx_pkts = tx_pkts = clients = 0
    for i in range(1, len(rows)):
        dt = int(rows[i]["ts"]) - int(rows[i - 1]["ts"])
        if dt <= 0:
            continue
        drx = max(0, int(rows[i]["rx_bytes"]) - int(rows[i - 1]["rx_bytes"]))
        dtx = max(0, int(rows[i]["tx_bytes"]) - int(rows[i - 1]["tx_bytes"]))
        bw_samples.append((drx + dtx) * 8 / dt / 1e6)  # Mbps
    if rows:
        rx_pkts = int(rows[-1]["rx_packets"])
        tx_pkts = int(rows[-1]["tx_packets"])
        clients = max(int(r["clients"]) for r in rows)

    # proc.csv: ts,sfu_cpu,sfu_rss,sig_cpu,sig_rss
    procs = []
    with open(os.path.join(report_dir, "proc.csv")) as f:
        for r in csv.DictReader(f):
            procs.append(r)
    sfu_cpu = [float(r["sfu_cpu"]) for r in procs if r.get("sfu_cpu")]
    sfu_rss = [float(r["sfu_rss"]) / 1024 for r in procs if r.get("sfu_rss")]  # KB -> MB
    sig_cpu = [float(r["sig_cpu"]) for r in procs if r.get("sig_cpu")]
    sig_rss = [float(r["sig_rss"]) / 1024 for r in procs if r.get("sig_rss")]

    # 实际流帧率：解析观看端 /tmp/load-view-*.log 的 "RECEIVED: N frames" 累计计数
    # （loadtest.sh 固定输出路径）。
    try:
        all_received = []
        for logf in glob.glob("/tmp/load-view-*.log"):
            try:
                with open(logf) as lf:
                    for line in lf:
                        m = re.search(r"(\d{2}):(\d{2}):(\d{2}).*RECEIVED: (\d+) frames", line)
                        if m:
                            h, mi, se, n = map(int, m.groups())
                            all_received.append((h * 3600 + mi * 60 + se, n))
            except OSError:
                pass
        eff_fps = 0.0
        if all_received:
            (t0, f0), (t1, f1) = all_received[0], all_received[-1]
            dur = max(t1 - t0, 1)
            eff_fps = round((f1 - f0) / dur, 2)
    except Exception:
        eff_fps = 0.0

    # 端到端延迟（#8）：viewer 日志 "LATENCY: N ms"（cursor 通道带发送时间戳）。
    latencies = []
    try:
        for logf in glob.glob("/tmp/load-view-*.log"):
            try:
                with open(logf) as lf:
                    for line in lf:
                        m = re.search(r"LATENCY: (\d+) ms", line)
                        if m:
                            latencies.append(int(m.group(1)))
            except OSError:
                pass
    except Exception:
        latencies = []
    lat_sorted = sorted(latencies)
    latency = {
        "avg": round(sum(lat_sorted) / len(lat_sorted), 1) if lat_sorted else None,
        "max": lat_sorted[-1] if lat_sorted else None,
        "p99": lat_sorted[int(len(lat_sorted) * 0.99)] if lat_sorted else None,
        "samples": len(lat_sorted),
    }

    # loadtest.log 连接统计
    load = ""
    try:
        with open(os.path.join(report_dir, "loadtest.log")) as f:
            load = f.read()
    except FileNotFoundError:
        pass
    m_pub = re.search(r"发布端: (\d+) 个连接成功", load)
    m_view = re.search(r"观看端: (\d+) 个连接成功", load)

    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "machine": machine_info(),
        "config": {
            "rooms": rooms,
            "pairs": pairs,
            "seconds": seconds,
            "resolution": f"{w}x{h}",
            "fps": fps,
            "bitrate_bps": bitrate,
            "expected_sessions": rooms * pairs * 2,
        },
        "results": {
            "effective_stream_fps": eff_fps,
            "latency_ms": latency,
            "publishers_connected": int(m_pub.group(1)) if m_pub else None,
            "viewers_connected": int(m_view.group(1)) if m_view else None,
            "peak_clients": clients,
            "bandwidth_mbps": {
                "avg": round(sum(bw_samples) / len(bw_samples), 2) if bw_samples else 0,
                "max": round(max(bw_samples), 2) if bw_samples else 0,
                "samples": len(bw_samples),
            },
            "rx_packets": rx_pkts,
            "tx_packets": tx_pkts,
            "sfu": {
                "cpu_avg": avg_max(sfu_cpu)[0],
                "cpu_max": avg_max(sfu_cpu)[1],
                "rss_mb_avg": avg_max(sfu_rss)[0],
                "rss_mb_max": avg_max(sfu_rss)[1],
            },
            "signal": {
                "cpu_avg": avg_max(sig_cpu)[0],
                "cpu_max": avg_max(sig_cpu)[1],
                "rss_mb_avg": avg_max(sig_rss)[0],
                "rss_mb_max": avg_max(sig_rss)[1],
            },
        },
    }
    with open(os.path.join(report_dir, "report.json"), "w") as f:
        json.dump(report, f, ensure_ascii=False, indent=2)

    r = report["results"]
    print("## AeroDesk 压测报告")
    print()
    print(f"- 时间：{report['generated_at']}")
    print(f"- 机器：{report['machine']}")
    print(f"- 配置：{rooms} 房间 × {pairs} 对 @ {w}x{h}/{fps}fps {bitrate // 1000000}Mbps，{seconds}s")
    print()
    print("| 指标 | 值 |")
    print("|---|---|")
    print(f"| 发布端连接 | {r['publishers_connected']} / {report['config']['expected_sessions'] // 2} |")
    print(f"| 观看端连接 | {r['viewers_connected']} / {report['config']['expected_sessions'] // 2} |")
    print(f"| 实际流帧率 | {r['effective_stream_fps']} fps（目标 {report['config']['fps']}）|")
    if r['latency_ms']['samples']:
        lat = r['latency_ms']
        print(f"| 端到端延迟 | avg {lat['avg']} ms / max {lat['max']} ms / p99 {lat['p99']} ms（{lat['samples']} 样本）|")
    else:
        print("| 端到端延迟 | 无样本（发布端未启用 cursor 通道） |")
    print(f"| 峰值客户端 | {r['peak_clients']} |")
    print(f"| 吞吐 | {r['bandwidth_mbps']['avg']} Mbps 平均 / {r['bandwidth_mbps']['max']} Mbps 峰值 |")
    print(f"| 收/发包 | {r['rx_packets']} / {r['tx_packets']} |")
    print(f"| SFU CPU | {r['sfu']['cpu_avg']}% 平均 / {r['sfu']['cpu_max']}% 峰值 |")
    print(f"| SFU 内存 | {r['sfu']['rss_mb_avg']} MB 平均 / {r['sfu']['rss_mb_max']} MB 峰值 |")
    print(f"| signal CPU | {r['signal']['cpu_avg']}% 平均 / {r['signal']['cpu_max']}% 峰值 |")
    print(f"| signal 内存 | {r['signal']['rss_mb_avg']} MB 平均 / {r['signal']['rss_mb_max']} MB 峰值 |")
    print()
    print("> p99 抖动/端到端延迟由 netem 回归（crates/aerodesk-sfu/tests/sim.rs）与真机矩阵覆盖。")
    print(f"完整数据：{report_dir}/report.json")


if __name__ == "__main__":
    main()
