"""A complete container can be used while the full page exceeds its budget."""
import http.server
import os
import tempfile
import threading
import time
import json
import verify_mcp_browser_session as fixture


def main():
    fixture.LOGIN = """<!doctype html><title>Fixture login</title><body>
    <p>OUTSIDE_SCOPE_SENTINEL</p><table><tr><td>OUTSIDE_TABLE</td></tr></table><button>Run</button><div id="noise"></div><section id="work" aria-label="Work">
    <button onclick="this.textContent='Done'">Run</button><table><tr><td>INSIDE_TABLE</td></tr></table></section>
    <section id="duplicates"><button>Same</button><button>Same</button></section>
    <section id="replace"><button onclick="setTimeout(()=>document.querySelector('#replace').outerHTML=document.querySelector('#replace').outerHTML,800)">Rebuild</button></section>
    <div id="shadow"></div><section id="shadowScope"><div id="shadowHost"></div></section><iframe srcdoc="<section id='work'><button>Frame</button></section>"></iframe>
    <script>noise.innerHTML='<p>noise</p>'.repeat(2100);
    shadowHost.attachShadow({mode:'open'}).innerHTML='<section aria-label="Shadow region">INSIDE_SHADOW<table><tr><td>SHADOW_TABLE</td></tr></table></section>';
    shadow.attachShadow({mode:'open'}).innerHTML='<button>Shadow</button>';</script>"""
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), fixture.Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="aad-scope-") as root:
        client = fixture.Client(dict(os.environ, AAD_RECORDINGS_DIR=root))
        try:
            client.tool("browser_open", {"browser": os.environ.get("AAD_TEST_BROWSER", "edge"),
                "headless": True, "url": f"http://127.0.0.1:{server.server_address[1]}/login"})
            whole = fixture.observe(client, "Fixture login")
            assert whole["truncated"] is True, whole
            resource = whole["resource_id"]
            def observe_scope(scope, limit=1000, rid=None):
                value, _ = client.tool("control_observe", {"resource_id": rid or resource,
                    "max_nodes": limit, "view": {"mode": "nodes", "scope_id": scope}})
                return value["data"]
            def rejected(name, arguments, code):
                result = client.request("tools/call", {"name": name, "arguments": arguments})["result"]
                assert result.get("isError") and code in json.dumps(result), result
            text, _ = client.tool("control_observe", {"resource_id":resource,
                "view":{"mode":"text","scope_id":"work"}})
            text=text["data"]
            assert "OUTSIDE" not in json.dumps(text), text
            assert "INSIDE_TABLE" in text["text"] and text["table_count"] == 1, text
            assert text["characters"] == len(text["text"]) and text["characters"] < 100, text
            full, _ = client.tool("control_observe", {"resource_id":resource,"view":{"mode":"text"}})
            assert "OUTSIDE_SCOPE_SENTINEL" in full["data"]["text"], full
            shadow_text,_=client.tool("control_observe", {"resource_id":resource,
                "view":{"mode":"text","scope_id":"shadowScope","region":"Shadow region"}})
            assert "INSIDE_SHADOW" in shadow_text["data"]["text"],shadow_text
            assert shadow_text["data"]["table_count"]==1,shadow_text
            assert "SHADOW_TABLE" in json.dumps(shadow_text["data"]["tables"]),shadow_text
            assert "OUTSIDE" not in json.dumps(shadow_text),shadow_text
            duplicates = observe_scope("duplicates")
            rejected("control_find", {"observation_id": duplicates["observation_id"],
                "locator": {"name": "Same", "role": "button"}}, "CONTROL.AMBIGUOUS")
            truncated = observe_scope("noise", 5)
            assert truncated["truncated"]
            rejected("control_find", {"observation_id": truncated["observation_id"],
                "locator": {"name": "noise"}}, "CONTROL.OBSERVATION_TRUNCATED")
            shadow = observe_scope("shadow")
            target = next(n["target"] for n in shadow["nodes"] if n["name"] == "Shadow")
            client.tool("control_act", {"target": target, "action": {"type": "click"}})
            rebuilding = observe_scope("replace")
            target = next(n["target"] for n in rebuilding["nodes"] if n["role"] == "button")
            client.tool("control_act", {"target": target, "action": {"type": "click"}})
            rebuilding = observe_scope("replace")
            target = next(n["target"] for n in rebuilding["nodes"] if n["role"] == "button")
            time.sleep(1)
            rejected("control_find", {"observation_id": rebuilding["observation_id"],
                "locator": {"name": "Rebuild", "role": "button"}}, "CONTROL.STALE_TARGET")
            rejected("control_act", {"target": target, "action": {"type": "click"}}, "CONTROL.STALE_TARGET")
            previous = observe_scope("work")
            observe_scope("work")
            rejected("control_find", {"observation_id": previous["observation_id"],
                "locator": {"name": "Run"}}, "CONTROL.STALE_TARGET")
            resources, _ = client.tool("control_targets", {})
            frame = next(r for r in resources["data"]["resources"] if r["provider"] == "browser.cdp" and not r["metadata"].get("main_frame"))
            frame_view = observe_scope("work", rid=frame["resource_id"])
            assert any(n["name"] == "Frame" for n in frame_view["nodes"]), frame_view
            assert not any(n["name"] == "Run" for n in frame_view["nodes"]), frame_view
            scoped, _ = client.tool("control_observe", {"resource_id": resource,
                "view": {"mode": "nodes", "scope_id": "work"}})
            scoped = scoped["data"]
            assert not scoped["truncated"], scoped
            found, _ = client.tool("control_find", {"observation_id": scoped["observation_id"],
                "locator": {"name": "Run", "role": "button"}})
            verified, _ = client.tool("control_act", {"target": found["data"]["target"], "action": {"type": "click"}, "verify":{"name":"Done","role":"button"}})
            assert verified["data"]["verified"], verified
            after, _ = client.tool("control_observe", {"resource_id": resource,
                "view": {"mode": "nodes", "scope_id": "work"}})
            assert any(n["name"] == "Done" for n in after["data"]["nodes"]), after
        finally:
            client.close()
            server.shutdown()
            server.server_close()
    print("PASS: complete scoped capture and unique action on a truncated page")


if __name__ == "__main__":
    main()
