#!/usr/bin/env python3
"""
Profile memory (RSS), CPU %, network bandwidth, RTT, and frame stats
for gutted-host (including WPE subprocesses) and gutted-client-gtk / gutted-client.
Supports both local host and remote host (--server <addr> --pin <sha256>).
"""

import argparse
import json
import os
import re
import subprocess
import time
import psutil

ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")


def strip_ansi(text):
    return ANSI_RE.sub("", text)


def get_process_tree(pid):
    try:
        parent = psutil.Process(pid)
        return [parent] + parent.children(recursive=True)
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return []


def profile_run(
    url,
    duration_secs=5,
    client_bin="./target/release/gutted-client-gtk",
    remote_server=None,
    remote_pin=None,
):
    subprocess.run(["cargo", "build", "--release", "--quiet"], check=True)

    os.system("pkill -9 -x gutted-client-gtk 2>/dev/null || true")
    os.system("pkill -9 -x gutted-client 2>/dev/null || true")
    if not remote_server:
        os.system("pkill -9 -x gutted-host 2>/dev/null || true")
    time.sleep(0.3)

    host_log_path = "/tmp/gutted-profile-host.log"
    client_log_path = "/tmp/gutted-profile-client.log"
    host_proc = None

    if remote_server:
        server_addr = remote_server
        pin = remote_pin
    else:
        server_addr = "127.0.0.1:4433"
        host_proc = subprocess.Popen(
            ["./target/release/gutted-host"],
            env=dict(os.environ, GBROWSER_URL=url, GBROWSER_LISTEN=server_addr, NO_COLOR="1"),
            stdout=open(host_log_path, "w"),
            stderr=subprocess.STDOUT,
        )

        pin = None
        for _ in range(40):
            if host_proc.poll() is not None:
                print("Host failed to start")
                return None
            if os.path.exists(host_log_path):
                with open(host_log_path, "r") as f:
                    for line in f:
                        if "GBROWSER_CERT_SHA256=" in line:
                            pin = line.strip().split("=")[1]
                            break
            if pin:
                break
            time.sleep(0.1)

        if not pin:
            print("Failed to find cert pin")
            host_proc.kill()
            return None

    client_env = dict(
        os.environ,
        GBROWSER_SERVER=server_addr,
        GBROWSER_NAV=url,
        GBROWSER_HOLD_SECS=str(duration_secs),
        NO_COLOR="1",
    )
    if pin:
        client_env["GBROWSER_CERT_SHA256"] = pin

    client_proc = subprocess.Popen(
        [client_bin],
        env=client_env,
        stdout=open(client_log_path, "w"),
        stderr=subprocess.STDOUT,
    )

    host_cpu_samples = []
    host_rss_samples = []
    client_cpu_samples = []
    client_rss_samples = []

    last_host_times = {}
    last_client_times = {}
    last_ts = time.time()

    start_time = time.time()
    while time.time() - start_time < duration_secs:
        time.sleep(0.25)
        now = time.time()
        dt = max(now - last_ts, 0.001)
        last_ts = now

        if host_proc is not None:
            host_procs = get_process_tree(host_proc.pid)
            if host_procs:
                total_host_cpu = 0.0
                total_host_rss = 0.0
                for p in host_procs:
                    try:
                        t = p.cpu_times()
                        total_time = t.user + t.system
                        prev = last_host_times.get(p.pid, total_time)
                        cpu_p = ((total_time - prev) / dt) * 100.0
                        last_host_times[p.pid] = total_time
                        total_host_cpu += max(cpu_p, 0.0)
                        total_host_rss += p.memory_info().rss / (1024 * 1024)
                    except (psutil.NoSuchProcess, psutil.AccessDenied):
                        pass
                host_cpu_samples.append(total_host_cpu)
                host_rss_samples.append(total_host_rss)

        client_procs = get_process_tree(client_proc.pid)
        if client_procs:
            total_client_cpu = 0.0
            total_client_rss = 0.0
            for p in client_procs:
                try:
                    t = p.cpu_times()
                    total_time = t.user + t.system
                    prev = last_client_times.get(p.pid, total_time)
                    cpu_p = ((total_time - prev) / dt) * 100.0
                    last_client_times[p.pid] = total_time
                    total_client_cpu += max(cpu_p, 0.0)
                    total_client_rss += p.memory_info().rss / (1024 * 1024)
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    pass
            client_cpu_samples.append(total_client_cpu)
            client_rss_samples.append(total_client_rss)

    try:
        client_proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        client_proc.kill()
        client_proc.wait()

    if host_proc is not None:
        try:
            host_proc.kill()
            host_proc.wait()
        except Exception:
            pass

    frames_full = 0
    frames_sub = 0
    bytes_total = 0
    quic_rtt = None
    welcome_rtt_us = None

    if os.path.exists(client_log_path):
        with open(client_log_path, "r") as f:
            for raw_line in f:
                line = strip_ansi(raw_line)
                m_rtt = re.search(r"rtt=([0-9.a-zµμ]+)", line)
                if m_rtt and quic_rtt is None:
                    quic_rtt = m_rtt.group(1)
                m_wel = re.search(r"welcome_rtt_us=(\d+)", line)
                if m_wel:
                    welcome_rtt_us = int(m_wel.group(1))
                m_ff = re.search(r"frames_full=(\d+)", line)
                if m_ff:
                    frames_full = max(frames_full, int(m_ff.group(1)))
                m_fs = re.search(r"frames_sub=(\d+)", line)
                if m_fs:
                    frames_sub = max(frames_sub, int(m_fs.group(1)))
                m_cb = re.search(r"cum_bytes=(\d+)", line)
                if m_cb:
                    bytes_total = max(bytes_total, int(m_cb.group(1)))
                m_wt = re.search(r"wire_total=(\d+)", line)
                if m_wt:
                    bytes_total = max(bytes_total, int(m_wt.group(1)))

    if host_proc is not None and os.path.exists(host_log_path):
        with open(host_log_path, "r") as f:
            for raw_line in f:
                line = strip_ansi(raw_line)
                m_cb = re.search(r"cum_bytes=(\d+)", line)
                if m_cb:
                    bytes_total = max(bytes_total, int(m_cb.group(1)))

    result = {
        "server": server_addr,
        "url": url,
        "duration_secs": duration_secs,
        "quic_handshake_rtt": quic_rtt,
        "welcome_rtt_us": welcome_rtt_us,
        "client_cpu_avg": round(sum(client_cpu_samples) / max(len(client_cpu_samples), 1), 2),
        "client_cpu_peak": round(max(client_cpu_samples) if client_cpu_samples else 0, 2),
        "client_rss_avg_mb": round(sum(client_rss_samples) / max(len(client_rss_samples), 1), 2),
        "client_rss_peak_mb": round(max(client_rss_samples) if client_rss_samples else 0, 2),
        "frames_full": frames_full,
        "frames_sub": frames_sub,
        "bandwidth_kbps": round((bytes_total * 8) / (duration_secs * 1000), 2) if bytes_total else 0,
        "bytes_total": bytes_total,
    }
    if host_proc is not None:
        result["host_cpu_avg"] = round(sum(host_cpu_samples) / max(len(host_cpu_samples), 1), 2)
        result["host_cpu_peak"] = round(max(host_cpu_samples) if host_cpu_samples else 0, 2)
        result["host_rss_avg_mb"] = round(sum(host_rss_samples) / max(len(host_rss_samples), 1), 2)
        result["host_rss_peak_mb"] = round(max(host_rss_samples) if host_rss_samples else 0, 2)
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", default=None, help="Remote server host:port")
    parser.add_argument("--pin", default=None, help="Remote server cert SHA256 hex")
    parser.add_argument("--duration", type=int, default=5, help="Duration per URL in seconds")
    parser.add_argument("--client", default="./target/release/gutted-client-gtk", help="Client binary")
    args = parser.parse_args()

    urls = [
        "https://news.ycombinator.com",
        "https://www.wikipedia.org",
        "https://duckduckgo.com",
    ]
    for u in urls:
        print(f"Profiling {u} against {args.server or 'local host'}...")
        res = profile_run(
            u,
            duration_secs=args.duration,
            client_bin=args.client,
            remote_server=args.server,
            remote_pin=args.pin,
        )
        if res:
            print(json.dumps(res, indent=2))
