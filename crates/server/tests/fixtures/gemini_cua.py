#!/usr/bin/env python3
"""Small stateful SwiftCUA stand-in. It never accesses a desktop or network."""
import json
from pathlib import Path
import sys

method, args = sys.argv[2], json.loads(sys.argv[3])
path = Path(__file__).with_suffix(".json")
s = json.loads(path.read_text()) if path.exists() else {
    "sidebar": False, "chat": 0, "draft": "", "sent": "", "menu": False,
    "retried": False, "busy": 0, "model": "hydrogen", "submitted": [],
    "conversations": [[]]
}

def button(i, identifier, label="", selected=False):
    return dict(index=i, role="AXButton", identifier=identifier, description=label,
                states=["enabled", "selected"] if selected else ["enabled"])

if method == "getAppState":
    if not s["busy"]:
        s["busy"] = 1
        path.write_text(json.dumps(s))
        print("[1007] AX error -25204", file=sys.stderr)
        sys.exit(1)
    controls = [button(1, "tab_stop_model_selector", s["model"]), button(2, "sidebar_toggle"),
                dict(index=3, role="AXTextArea", value=s["draft"]),
                button(4, "send_button"), button(6, "newChat_button" if s["sent"] else "tempChat_button")]
    if s["menu"]:
        controls.append(dict(role="AXPopover", children=[
            button(5, None, s["model"], True), button(10, None, "思考レベル"),
            button(11, None, "高、高速")]))
    if s["sidebar"]:
        controls.append(button(7, "sidebar_header_search"))
        for chat in range(len(s["conversations"])):
            controls.append(button(100 + chat, f"sidebar.conversation.{chat}", selected=chat == s["chat"]))
    history = s["conversations"][s["chat"]]
    for turn, prompt in enumerate(history):
        controls.append(dict(role="AXHeading", description=prompt))
        if turn < len(history) - 1 or s["retried"]:
            children = [dict(role="AXStaticText", value="回答" * 4000 + " @all")]
        else:
            children = [dict(role="AXStaticText", value="エラーが発生しました（1213）"), button(9, None, "再試行")]
        controls.append(dict(role="AXRow", children=children))
    def filtered(node, query):
        children = [item for child in node.get("children", []) if (item := filtered(child, query))]
        if children or any(query in str(node.get(k) or "") for k in ["role", "identifier", "description", "value"]):
            return dict(node, children=children)
    q = args.get("query")
    if q:
        controls = [item for node in controls if (item := filtered(node, q))]
    print(json.dumps({"state":{"info":{"window":{"id":1}},"accessibility":{"nodes":controls}}}))
elif method == "setValue":
    assert args["elementIndex"] == 3
    s["draft"] = args["value"]
    print('{"accepted":true}')
elif method == "click":
    i = args["elementIndex"]
    if i == 1: s["menu"] = True
    elif i == 2: s["sidebar"] = not s["sidebar"]
    elif i == 4:
        s["sent"], s["draft"] = s["draft"], ""
        s["submitted"].append(s["sent"])
        s["conversations"][s["chat"]].append(s["sent"])
        s["retried"] = False
    elif i == 5 or i == 11: s["menu"] = False
    elif i == 6:
        s["chat"] = len(s["conversations"])
        s["conversations"].append([])
        s["sent"], s["retried"] = "", False
    elif i == 9: s["retried"] = True
    elif i >= 100:
        s["chat"] = i - 100
        history = s["conversations"][s["chat"]]
        s["sent"] = history[-1] if history else ""
        s["retried"] = True
    print('{"accepted":true}')
else:
    raise AssertionError(method)
path.write_text(json.dumps(s))
