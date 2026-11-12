"""Teste sem navegador: vídeo/áudio, oferta WebRTC e parada com espectador ativo.

Uso: python windows/test-server.py caminho/do/telinha[.exe]
Somente biblioteca padrão; também pode ser usado no Linux.
"""
import base64
import json
import os
import socket
import struct
import subprocess
import sys
import time


def main():
    binary = sys.argv[1] if len(sys.argv) > 1 else "target/debug/telinha"
    server = subprocess.Popen(
        [binary, "--test", "--audio", "--encoder", "x264enc", "--fps", "30",
         "--bind", "127.0.0.1", "--port", "18080"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, encoding="utf-8",
    )
    connection = stream = None
    try:
        for _ in range(60):
            if server.poll() is not None:
                raise RuntimeError(server.stdout.read())
            try:
                connection = socket.create_connection(("127.0.0.1", 18080), .25)
                break
            except OSError:
                time.sleep(.1)
        assert connection, "servidor nao iniciou"
        connection.settimeout(15)
        key = base64.b64encode(os.urandom(16)).decode()
        connection.sendall((
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nUpgrade: websocket\r\n"
            "Connection: Upgrade\r\nSec-WebSocket-Key: " + key +
            "\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode())
        stream = connection.makefile("rb")
        status = stream.readline()
        assert b"101" in status, status
        while True:
            header = stream.readline()
            assert header, "conexao encerrada durante handshake"
            if header == b"\r\n":
                break

        def send_text(text):
            # Quadros do cliente precisam de mascara (RFC 6455).
            payload = text.encode()
            mask = os.urandom(4)
            size = len(payload)
            if size < 126:
                header = bytes([0x81, 0x80 | size])
            else:
                header = bytes([0x81, 0x80 | 126]) + struct.pack("!H", size)
            masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
            connection.sendall(header + mask + masked)

        # A pagina se apresenta (nome, aparelho, id do navegador) antes do video.
        send_text(json.dumps({"type": "hello", "name": "Teste", "device": "CI", "client": "test-server"}))

        def exact(size):
            data = stream.read(size)
            assert len(data) == size, "websocket encerrado antes da oferta"
            return data

        for _ in range(10):
            first, length = exact(2)
            length &= 127
            if length == 126:
                length = struct.unpack("!H", exact(2))[0]
            elif length == 127:
                length = struct.unpack("!Q", exact(8))[0]
            data = exact(length)
            if first & 15 == 1:
                message = json.loads(data)
                if message["type"] == "offer":
                    assert "m=audio" in message["sdp"] and "m=video" in message["sdp"]
                    break
        else:
            raise RuntimeError("oferta WebRTC ausente")
        server.stdin.write("q\n")
        server.stdin.flush()
        assert server.wait(timeout=5) == 0
        print("PASS: video/audio sinteticos, oferta WebRTC e parada com espectador ativo")
    finally:
        if stream:
            stream.close()
        if connection:
            connection.close()
        if server.poll() is None:
            server.kill()
            server.wait()
        print(server.stdout.read())


if __name__ == "__main__":
    main()
