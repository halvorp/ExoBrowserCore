#!/usr/bin/env python3
"""
Profile memory (RSS), CPU %, network bandwidth, and frame stats
for gutted-host (including WPE subprocesses) and gutted-client-gtk.
"""

import subprocess
import time
import os
import sys
import psutil
import json

def get_process_tree(pid):
    try:
        parent = psutil.Process(pid)
        return [parent] + parent.children(recursive=True)
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return []

def profile_run(url, duration_secs=6, client_bin="./target/release/gutted-client-gtk"):
    # Ensure release binaries exist
    subprocess.run(["cargo", "build", "--release", "--quiet"], check=True)
    
    # Kill any lingering processes
    os.system("pkill -9 -f 'target/release/gutted-host' 2>/dev/null || true")
    os.system("pkill -9 -f 'target/release/gutted-client-gtk' 2>/dev/null || true")
    os.system("pkill -9 -f 'target/release/gutted-client' 2>/dev/null || true")
    time.sleep(0.3)

    log_path = "/tmp/gutted-profile-host.log"
    host_proc = subprocess.Popen(
        ["./target/release/gutted-host"],
        env=dict(os.environ, GBROWSER_URL=url, GBROWSER_LISTEN="127.0.0.1:4433"),
        stdout=open(log_path, "w"),
        stderr=subprocess.STDOUT
    )

    # Wait for cert pin
    pin = None
    for _ in range(40):
        if host_proc.poll() is not None:
            print("Host failed to start")
            return None
        if os.path.exists(log_path):
            with open(log_path, "r") as f:
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

    # Start client
    client_proc = subprocess.Popen(
        [client_bin],
        env=dict(os.environ, GBROWSER_CERT_SHA256=pin, GBROWSER_SERVER="127.0.0.1:4433", GBROWSER_HOLD_SECS=str(duration_secs)),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL
    )

    # Samples
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
        
        # Host tree
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

        # Client tree
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

    # Clean up
    try:
        client_proc.kill()
        host_proc.kill()
        client_proc.wait()
        host_proc.wait()
    except Exception:
        pass

    # Read network / frame stats from host log
    frames_full = 0
    frames_sub = 0
    bytes_total = 0
    import re
    with open(log_path, "r") as f:
        for line in f:
            if "WPE frame → bus" in line:
                if "FULL" in line:
                    frames_full += 1
                elif "SUB" in line:
                    frames_sub += 1
            m = re.search(r'cum_bytes=(\d+)', line)
            if m:
                bytes_total = max(bytes_total, int(m.group(1)))

    return {
        "url": url,
        "duration_secs": duration_secs,
        "host_cpu_avg": sum(host_cpu_samples) / max(len(host_cpu_samples), 1),
        "host_cpu_peak": max(host_cpu_samples) if host_cpu_samples else 0,
        "host_rss_avg_mb": sum(host_rss_samples) / max(len(host_rss_samples), 1),
        "host_rss_peak_mb": max(host_rss_samples) if host_rss_samples else 0,
        "client_cpu_avg": sum(client_cpu_samples) / max(len(client_cpu_samples), 1),
        "client_cpu_peak": max(client_cpu_samples) if client_cpu_samples else 0,
        "client_rss_avg_mb": sum(client_rss_samples) / max(len(client_rss_samples), 1),
        "client_rss_peak_mb": max(client_rss_samples) if client_rss_samples else 0,
        "frames_full": frames_full,
        "frames_sub": frames_sub,
        "bandwidth_kbps": (bytes_total * 8) / (duration_secs * 1000) if bytes_total else 0,
        "bytes_total": bytes_total,
    }

if __name__ == "__main__":
    urls = [
        "https://news.ycombinator.com",
        "https://www.wikipedia.org",
        "https://duckduckgo.com",
    ]
    results = []
    for u in urls:
        print(f"Profiling {u}...")
        res = profile_run(u, duration_secs=5)
        if res:
            results.append(res)
            print(json.dumps(res, indent=2))
