#!/usr/bin/env python3
# UDP shaping proxy: sit between client and host, add delay/loss/rate cap.
# Written because the Crostini kernel has no sch_netem, so we can't tc.
#
# Client dials 127.0.0.1:LISTEN. We forward to TARGET after applying:
#   - per-packet delay (with jitter)
#   - per-packet loss probability
#   - egress rate cap (token bucket, bytes/sec)
# Both directions are shaped independently but with the same params.
#
# Usage:
#   ./udp_shaper.py --listen 127.0.0.1:4434 --target 127.0.0.1:4433 \
#                   --delay-ms 100 --jitter-ms 20 --loss 0.02 --rate-kbps 1024

import argparse, asyncio, random, socket, time

class Shaper:
    def __init__(self, delay_ms, jitter_ms, loss, rate_kbps):
        self.delay = delay_ms / 1000.0
        self.jitter = jitter_ms / 1000.0
        self.loss = loss
        self.bps = rate_kbps * 1000 / 8  # bytes/sec
        self.tokens = self.bps
        self.last = time.monotonic()
        self.dropped = 0
        self.forwarded = 0
        self.bytes_out = 0

    async def gate(self, size):
        # 1) loss
        if random.random() < self.loss:
            self.dropped += 1
            return False
        # 2) rate (token bucket)
        now = time.monotonic()
        self.tokens = min(self.bps, self.tokens + (now - self.last) * self.bps)
        self.last = now
        if size > self.tokens:
            await asyncio.sleep((size - self.tokens) / self.bps)
            self.tokens = 0
        else:
            self.tokens -= size
        # 3) delay
        d = self.delay + random.uniform(-self.jitter, self.jitter)
        if d > 0: await asyncio.sleep(d)
        self.forwarded += 1
        self.bytes_out += size
        return True


class Proxy(asyncio.DatagramProtocol):
    """Client-facing endpoint. Per source addr, we spawn an upstream socket."""
    def __init__(self, target, c2s, s2c):
        self.target = target
        self.c2s = c2s  # client -> server shaper
        self.s2c = s2c  # server -> client shaper
        self.transport = None
        self.peers = {}  # client_addr -> UpstreamProtocol

    def connection_made(self, transport):
        self.transport = transport

    def datagram_received(self, data, addr):
        peer = self.peers.get(addr)
        if peer is None:
            peer = UpstreamProtocol(self, addr)
            self.peers[addr] = peer
            loop = asyncio.get_event_loop()
            asyncio.ensure_future(loop.create_datagram_endpoint(
                lambda: peer, remote_addr=self.target))
            # buffer until upstream ready
        asyncio.ensure_future(peer.send_shaped(data))


class UpstreamProtocol(asyncio.DatagramProtocol):
    """Per-client-peer upstream socket toward the real server."""
    def __init__(self, proxy, client_addr):
        self.proxy = proxy
        self.client_addr = client_addr
        self.transport = None
        self.pending = []

    def connection_made(self, transport):
        self.transport = transport
        for buf in self.pending:
            self.transport.sendto(buf)
        self.pending.clear()

    async def send_shaped(self, data):
        if not await self.proxy.c2s.gate(len(data)):
            return
        if self.transport is None:
            self.pending.append(data)
        else:
            self.transport.sendto(data)

    def datagram_received(self, data, _addr):
        # server -> shaper -> client
        asyncio.ensure_future(self._back(data))

    async def _back(self, data):
        if not await self.proxy.s2c.gate(len(data)):
            return
        if self.proxy.transport is not None:
            self.proxy.transport.sendto(data, self.client_addr)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("--listen",    default="127.0.0.1:4434")
    p.add_argument("--target",    default="127.0.0.1:4433")
    p.add_argument("--delay-ms",  type=float, default=0.0)
    p.add_argument("--jitter-ms", type=float, default=0.0)
    p.add_argument("--loss",      type=float, default=0.0)
    p.add_argument("--rate-kbps", type=float, default=1_000_000)  # effectively unlimited
    a = p.parse_args()

    lhost, lport = a.listen.split(":")
    thost, tport = a.target.split(":")
    target = (thost, int(tport))

    c2s = Shaper(a.delay_ms, a.jitter_ms, a.loss, a.rate_kbps)
    s2c = Shaper(a.delay_ms, a.jitter_ms, a.loss, a.rate_kbps)

    loop = asyncio.get_event_loop()
    proxy = Proxy(target, c2s, s2c)
    await loop.create_datagram_endpoint(lambda: proxy, local_addr=(lhost, int(lport)))

    print(f"[shaper] {a.listen} -> {a.target}"
          f"  delay={a.delay_ms}±{a.jitter_ms}ms loss={a.loss*100:.1f}% rate={a.rate_kbps:.0f}kbps",
          flush=True)

    last = time.monotonic()
    while True:
        await asyncio.sleep(2)
        now = time.monotonic()
        dt = now - last; last = now
        print(f"[shaper] c2s: {c2s.forwarded} fwd / {c2s.dropped} drop / {c2s.bytes_out/dt:.0f} B/s | "
              f"s2c: {s2c.forwarded} fwd / {s2c.dropped} drop / {s2c.bytes_out/dt:.0f} B/s",
              flush=True)
        c2s.forwarded = c2s.dropped = c2s.bytes_out = 0
        s2c.forwarded = s2c.dropped = s2c.bytes_out = 0


if __name__ == "__main__":
    asyncio.run(main())
