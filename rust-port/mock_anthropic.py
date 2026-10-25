"""Mock Anthropic Messages server: verifies the Rust transport's wire contract
(headers, body shape) and streams a canned two-turn SSE conversation."""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

TURNS = {"n": 0}

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_POST(self):
        try:
            self.handle_post()
        except Exception as e:
            import traceback, os
            with open(r"C:\Users\Administrator\hermes-rs\mock_debug.txt", "a", encoding="utf-8") as f:
                f.write(f"EXC: {e!r}\n{traceback.format_exc()}\n")
            raise

    def handle_post(self):
        assert self.path == "/v1/messages", self.path
        assert self.headers.get("x-api-key") == "test-key", "missing x-api-key"
        assert self.headers.get("anthropic-version") == "2023-06-01", "missing anthropic-version"
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert body["stream"] is True
        assert body["max_tokens"] == 16384
        assert body["system"], "system prompt must be extracted from messages"
        assert body["tools"][0]["name"] == "terminal"
        assert "input_schema" in body["tools"][0]
        if TURNS["n"] == 0:
            assert body["messages"][0]["content"][0]["type"] == "text"
            TURNS["n"] = 1
            events = [
                ("message_start", {"type": "message_start"}),
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "MOCK-OK "}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "running tool"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("content_block_start", {"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_mock", "name": "terminal", "input": {}}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":\"echo "}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "mock-anthropic\"}"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 1}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
                ("message_stop", {"type": "message_stop"}),
            ]
        else:
            # Second turn: the tool result must ride a user message.
            blocks = body["messages"][-1]["content"]
            assert blocks[0]["type"] == "tool_result", blocks
            assert "mock-anthropic" in blocks[0]["content"], blocks
            events = [
                ("message_start", {"type": "message_start"}),
                ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "MOCK-DONE"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
                ("message_stop", {"type": "message_stop"}),
            ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for name, data in events:
            self.wfile.write(f"event: {name}\ndata: {json.dumps(data)}\n\n".encode())
            self.wfile.flush()

HTTPServer(("127.0.0.1", 8123), H).serve_forever()
