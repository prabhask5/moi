#!/usr/bin/env python3
"""A stand-in for a tunnel's SSH origin: a WebSocket server (TLS) that carries a TCP byte stream in
binary messages, both ways — what Cloudflare Tunnel's `ssh://` ingress speaks and what devbox-ssh and
moi's own probe expect. Uses the `websockets` library, an implementation neither of them shares.

    ws-bridge.py CERT KEY LISTEN_PORT TARGET_PORT
"""
import asyncio
import ssl
import sys

import websockets


async def bridge(ws, target_port):
    reader, writer = await asyncio.open_connection("127.0.0.1", target_port)

    async def to_tcp():
        try:
            async for message in ws:
                if isinstance(message, str):
                    await ws.close(1003)
                    return
                writer.write(message)
                await writer.drain()
        finally:
            writer.close()

    async def to_ws():
        try:
            while data := await reader.read(32768):
                await ws.send(data)
        finally:
            await ws.close()

    await asyncio.gather(to_tcp(), to_ws(), return_exceptions=True)


async def main():
    cert, key, listen, target = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4])
    tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    tls.load_cert_chain(cert, key)
    async with websockets.serve(lambda ws: bridge(ws, target), "127.0.0.1", listen, ssl=tls):
        await asyncio.Future()


asyncio.run(main())
