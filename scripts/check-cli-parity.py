#!/usr/bin/env python3
"""Check source-derived native/CLI catalogs and Web transport operation coverage.

This is a conservative lexical checker, not a TypeScript compiler. It balances
calls and objects, keeps quoted/template tokens, and discards comments. Audited
dynamic forwarding expressions are pinned by expression and occurrence count;
new expressions fail closed. It cannot prove control flow, payload equivalence,
authorization, upload integrity, retries, or user-visible behavior. Run integration
checks separately. --binary checks the artifact's public capabilities document.
"""
import argparse
from collections import Counter
from dataclasses import dataclass
import itertools
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile


@dataclass
class Token:
    value: str
    line: int
    kind: str = "code"


def lex(source):
    result, i, line = [], 0, 1
    while i < len(source):
        c = source[i]
        if c.isspace():
            line += c == "\n"
            i += 1
            continue
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end
            continue
        if source.startswith("/*", i):
            end = source.find("*/", i + 2)
            end = len(source) if end < 0 else end + 2
            line += source[i:end].count("\n")
            i = end
            continue
        # JavaScript regex literals may contain quotes and comment-looking text.
        if c == "/" and result and result[-1].value in ("=", "(", "[", ",", ":", "return", "=>", "!", "?", "|", "&"):
            end, in_class = i + 1, False
            while end < len(source) and source[end] != "\n":
                if source[end] == "\\":
                    end += 2
                    continue
                if source[end] == "[":
                    in_class = True
                elif source[end] == "]":
                    in_class = False
                elif source[end] == "/" and not in_class:
                    end += 1
                    while end < len(source) and source[end].isalpha():
                        end += 1
                    result.append(Token(source[i:end], line, "regex"))
                    i = end
                    break
                end += 1
            if i == end:
                continue
        # Rust raw strings, including JSON catalog examples.
        raw = re.match(r'r(#+)?"', source[i:])
        if raw:
            closing = '"' + (raw[1] or "")
            end = source.find(closing, i + len(raw[0]))
            if end < 0:
                raise ValueError(f"unterminated raw string at line {line}")
            value = source[i + len(raw[0]):end]
            result.append(Token(value, line, "string"))
            end += len(closing)
        elif c in '\"`' or (c == "'" and re.match(r"'(?:\\.|[^'\\\n])*'", source[i:])):
            start, quote = i, c
            i += 1
            while i < len(source):
                if source[i] == "\\":
                    i += 2
                elif source[i] == quote:
                    break
                else:
                    i += 1
            if i >= len(source):
                raise ValueError(f"unterminated string at line {line}")
            raw_value = source[start:i + 1]
            if quote == '"':
                try:
                    value = json.loads(raw_value)
                except ValueError:
                    value = raw_value[1:-1]
            else:
                value = raw_value[1:-1]
            result.append(Token(value, line, "template" if quote == "`" else "string"))
            end = i + 1
            i = start
        else:
            word = re.match(r"[A-Za-z_$][\w$]*|\d+|\.\.\.|::|=>|\?\.", source[i:])
            value = word[0] if word else c
            result.append(Token(value, line))
            end = i + len(value)
        line += source[i:end].count("\n")
        i = end
    return result


def close(tokens, start):
    stack = []
    for i in range(start, len(tokens)):
        t = tokens[i]
        if t.kind != "code":
            continue
        if t.value in ("(", "{", "["):
            stack.append({"(": ")", "{": "}", "[": "]"}[t.value])
        elif t.value in (")", "}", "]"):
            if not stack or stack.pop() != t.value:
                raise ValueError(f"unbalanced delimiter at line {t.line}")
            if not stack:
                return i
    raise ValueError(f"unclosed delimiter at line {tokens[start].line}")


def split(tokens, delimiter=","):
    parts, start, i = [], 0, 0
    while i < len(tokens):
        t = tokens[i]
        if t.kind == "code" and t.value in ("(", "{", "["):
            i = close(tokens, i)
        elif t.kind == "code" and t.value == delimiter:
            parts.append(tokens[start:i])
            start = i + 1
        i += 1
    if tokens[start:]:
        parts.append(tokens[start:])
    return parts


def text(tokens):
    return " ".join(json.dumps(t.value) if t.kind != "code" else t.value for t in tokens)


def snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def enum_attribute(tokens, enum_index, key):
    j = enum_index - 1
    if j >= 0 and tokens[j].value == "pub":
        j -= 1
    while j >= 0 and tokens[j].value == "]":
        end, depth = j, 1
        j -= 1
        while j >= 0 and depth:
            if tokens[j].kind == "code":
                depth += tokens[j].value == "]"
                depth -= tokens[j].value == "["
            j -= 1
        if j < 0 or tokens[j].value != "#":
            break
        attr = tokens[j:end + 1]
        for k in range(len(attr) - 2):
            if attr[k].value == key and attr[k + 1].value == "=":
                return attr[k + 2].value
        j -= 1
    return None


def enum(tokens, name):
    for i in range(len(tokens) - 2):
        if [t.value for t in tokens[i:i + 2]] != ["enum", name]:
            continue
        body = i + 2
        if tokens[body].value == "<":
            depth = 1
            body += 1
            while body < len(tokens) and depth:
                token = tokens[body]
                if token.kind == "code":
                    depth += token.value == "<"
                    depth -= token.value == ">"
                body += 1
        if body >= len(tokens) or tokens[body].value != "{":
            raise ValueError(f"{name}: missing native enum body")
        rename = enum_attribute(tokens, i, "rename_all")
        if rename != "snake_case":
            raise ValueError(f"{name}: expected native snake_case enum serialization, found {rename!r}")
        result = {}
        for row in split(tokens[body + 1:close(tokens, body)]):
            j, tag = 0, None
            while j < len(row) and row[j].value == "#":
                end = close(row, j + 1)
                attr = row[j:end + 1]
                for k in range(len(attr) - 2):
                    if attr[k].value == "rename" and attr[k + 1].value == "=":
                        tag = attr[k + 2].value
                j = end + 1
            if j < len(row):
                variant = row[j].value
                result[variant] = {"tag": tag or snake(variant), "line": row[j].line,
                                   "fields": fields(row[j + 1:])}
        return result
    raise ValueError(f"missing native enum {name}")


def discriminator(tokens, name):
    for i in range(len(tokens) - 1):
        if [t.value for t in tokens[i:i + 2]] == ["enum", name]:
            return enum_attribute(tokens, i, "tag")
    raise ValueError(f"missing native enum {name}")


def fields(tokens):
    if tokens and tokens[0].value == "{":
        tokens = tokens[1:close(tokens, 0)]
    result = []
    for row in split(tokens):
        if row and (len(row) == 1 or (len(row) > 1 and row[1].value == ":")):
            if re.fullmatch(r"[\w$]+", row[0].value):
                result.append(row[0].value)
    return sorted(set(result))


def macros(tokens, name):
    for i in range(len(tokens) - 2):
        if [t.value for t in tokens[i:i + 3]] == [name, "!", "("]:
            yield tokens[i + 3:close(tokens, i + 2)]


def source_capabilities(tokens):
    operations, nested = [], {}
    for body in macros(tokens, "operation_catalog"):
        header, *rows = split(body, ";")
        kind = split(header)[2][0].value
        for row in rows:
            columns = split(row)
            operations.append({"kind": kind, "native_variant": columns[0][0].value,
                               "command": f"client {kind} {columns[2][0].value}",
                               "tag": columns[3][0].value, "transport": columns[4][0].value,
                               "core_methods": [t.value for t in columns[5] if t.kind == "string"]})
    for body in macros(tokens, "nested_catalog"):
        header, *rows = split(body, ";")
        columns = split(header)
        native = columns[2][0].value
        variants = []
        for row in rows:
            arrow = next(i for i, t in enumerate(row) if t.value == "=>")
            columns = split(row[arrow + 1:])
            variants.append({"native_variant": columns[0][0].value, "tag": columns[1][0].value})
        nested[native] = {"tag": split(header)[3][0].value,
                          "variants": variants}
    return {"schema_version": 1, "operations": operations, "nested": nested}


def lifecycle(tokens):
    for i in range(len(tokens) - 2):
        if tokens[i].value == "local_lifecycle" and tokens[i + 1].value == ":":
            body = tokens[i + 3:close(tokens, i + 2)]
            entries = []
            for row in split(body):
                item = {}
                for prop in split(row[1:close(row, 0)]):
                    if len(prop) < 3 or prop[1].value != ":":
                        continue
                    if prop[0].value == "command":
                        item["command"] = prop[2].value
                    elif prop[0].value == "core_methods":
                        item["core_methods"] = [t.value for t in prop[3:-1] if t.kind == "string"]
                entries.append(item)
            return entries
    raise ValueError("capabilities source missing local_lifecycle")


@dataclass
class Call:
    callee: str
    args: list
    line: int
    signature: str


def calls(tokens):
    """Only actual invocations, excluding function/method declarations."""
    for i, token in enumerate(tokens):
        if token.kind != "code" or token.value != "(":
            continue
        j = i - 1
        if j < 0:
            continue
        # Generic call arguments such as rpc<Response>(...).
        if tokens[j].value == ">":
            depth = 1
            j -= 1
            while j >= 0 and depth:
                depth += tokens[j].value == ">"
                depth -= tokens[j].value == "<"
                j -= 1
        if j < 0 or not re.fullmatch(r"[\w$]+", tokens[j].value):
            continue
        start = j
        while start >= 2 and tokens[start - 1].value in (".", "?."):
            start -= 2
        if start and tokens[start - 1].value == "function":
            continue
        end = close(tokens, i)
        # Object/class method signatures and interfaces are not calls.
        if end + 1 < len(tokens) and tokens[end + 1].value == "{":
            continue
        if end + 1 < len(tokens) and tokens[end + 1].value == ":" and (
                any(t.value == ":" for t in tokens[i + 1:end]) or
                (end + 2 < len(tokens) and tokens[end + 2].value == "Promise")):
            continue
        callee = "".join(t.value for t in tokens[start:j + 1])
        args = split(tokens[i + 1:end])
        yield Call(callee, args, tokens[start].line, callee + "(" + text(tokens[i + 1:end]) + ")")
    # Identity intentionally returns a scoped transport function.
    for i in range(len(tokens) - 1):
        if tokens[i].value != "scoped" or tokens[i + 1].value != "(":
            continue
        end = close(tokens, i + 1)
        if end + 1 < len(tokens) and tokens[end + 1].value == "(":
            stop = close(tokens, end + 1)
            yield Call("scoped()", split(tokens[end + 2:stop]), tokens[i].line,
                       "scoped()(" + text(tokens[end + 2:stop]) + ")")


W = "packages/client/src/workspace/context.tsx"
I = "packages/client/src/identity/context.tsx"
D = "packages/client/src/delivery/client.ts"
M = "packages/client/src/workspace/memory.ts"

# Exact forwarding expressions, with audited occurrence counts. Concrete producer
# calls and outbox method fields in these files are independently inventoried.
# A new dynamic site is an error even when it resembles an existing bridge.
BRIDGES = {
    I: {
        'active.channel.request(method , params)': (1, "identity transport forwarding"),
        'request(method , params)': (1, "ReadyConnection request forwarding"),
        'callSession(active , method , params)': (1, "scoped identity transport forwarding"),
        'channel.request(method , params)': (1, "invite preflight forwarding; ask producers scanned"),
        'ask()': (1, "local Node-join retry wrapper; concrete callSession producer scanned"),
    },
    W: {
        'rpc(item . method , item . params)': (2, "validated durable outbox dispatch; method producers scanned"),
        'rpc(method , params)': (2, "guarded memory/history forwarding; producer modules scanned"),
        'connection.rpc(method , params)': (1, "ledger-guarded transport forwarding; concrete producers scanned"),
    },
    M: {
        'options.rpc(item . method , item . params)': (1, "memory outbox dispatch; method producer scanned"),
        'request(item)': (2, "local memory request validator, no network request"),
        'request(pending)': (1, "local memory request validator, no network request"),
    },
}
HELPERS = {
    "apps/app/src/features/devices/ai-settings.tsx": ("call", 2),
    "apps/app/src/features/devices/triage-settings.tsx": ("call", 1),
    "apps/app/src/features/devices/relay-settings.tsx": ("call", 1),
}
# Exact browser-local expressions, with audited occurrence counts.
LOCAL_CALLS = {
    (W, "notifier.request()"): (1, "platform notification permission; terminal events use client watch"),
    ("apps/app/src/lib/delivery-reservations.ts",
     'locks.request("vibeface-delivery-intent:${key}" , { mode : "exclusive" } , async ( ) => reserve ( ))'):
        (1, "browser lock for delivery intent reservation; no Core operation"),
}


def literal(tokens):
    return tokens[0].value if len(tokens) == 1 and tokens[0].kind == "string" else None


def object_properties(tokens):
    if not tokens or tokens[0].value != "{" or close(tokens, 0) != len(tokens) - 1:
        return None
    result = {}
    for row in split(tokens[1:-1]):
        if (len(row) < 3 or row[0].kind not in ("code", "string") or
                not re.fullmatch(r"[\w$]+", row[0].value) or row[1].value != ":" or
                row[0].value in result):
            return None
        result[row[0].value] = row[2:]
    return result


def web_inventory(root):
    sites, classified, errors, bridge_counts, local_counts, payload_sources = [], [], [], Counter(), Counter(), []
    files = sorted(itertools.chain((root / "packages/client/src").rglob("*.ts*"),
                                   (root / "apps/app/src").rglob("*.ts*")))
    for path in files:
        if path.suffix not in (".ts", ".tsx") or ".test." in path.name:
            continue
        rel = path.relative_to(root).as_posix()
        tokens = lex(path.read_text())
        file_calls = list(calls(tokens))
        for i, token in enumerate(tokens):
            if token.kind == "code" and token.value in ("attestation", "owner_attestation", "operation_id"):
                before = tokens[i - 1].value if i else ""
                after = tokens[i + 1].value if i + 1 < len(tokens) else ""
                if before in ("{", ",") and after in (":", ",", "}"):
                    payload_sources.append({"source": f"{rel}:{token.line}", "field": token.value,
                                            "note": "Observed object property; signing/validation behavior requires integration checks."})
        for call in file_calls:
            leaf = call.callee.split(".")[-1]
            index = None
            transport = leaf
            if leaf in ("rpc", "request"):
                index = 0
            elif call.callee == "signedWrite":
                index = 1
                transport = "rpc"
            elif rel == I and call.callee in ("callSession", "ask"):
                index = 1
                transport = "request"
            elif rel == I and call.callee == "scoped()":
                index = 0
                transport = "request"
            elif rel in HELPERS and call.callee == HELPERS[rel][0]:
                index = HELPERS[rel][1]
                transport = "request"
            else:
                continue
            if call.signature in BRIDGES.get(rel, {}):
                bridge_counts[(rel, call.signature)] += 1
                classified.append({"source": f"{rel}:{call.line}", "expression": call.signature,
                                   "reason": BRIDGES[rel][call.signature][1]})
                continue
            # Narrow browser-local APIs, selected by receiver and path.
            local = LOCAL_CALLS.get((rel, call.signature))
            local_reason = local[1] if local else None
            if local:
                local_counts[(rel, call.signature)] += 1
            if call.callee in ("navigator.locks.request", "globalThis.navigator.locks.request") and rel in (
                    "apps/app/src/lib/tabs.ts", "apps/app/src/lib/platform.ts"):
                local_reason = "browser lock for leader election/input serialization; no Core operation"
            if local_reason:
                classified.append({"source": f"{rel}:{call.line}", "expression": call.signature,
                                   "reason": local_reason})
                continue
            if index >= len(call.args):
                errors.append(f"{rel}:{call.line}: unclassified call {call.signature}")
                continue
            payload = call.args[index + 1] if len(call.args) > index + 1 else []
            expression = call.args[index]
            if call.callee == "signedWrite":
                operation = object_properties(expression)
                if operation is None or "method" not in operation or "params" not in operation:
                    errors.append(f"{rel}:{call.line}: unclassified dynamic call {call.signature}")
                    continue
                expression, payload = operation["method"], operation["params"]
            method = literal(expression)
            methods = [method] if method else []
            # Template union is obtained from the actual parameter declaration.
            if len(expression) == 1 and expression[0].kind == "template":
                template = expression[0].value
                names = re.findall(r"\$\{(\w+)\}", template)
                options = []
                for name in names:
                    unions = []
                    for i in range(len(tokens) - 2):
                        if tokens[i].value == name and tokens[i + 1].value == ":" and tokens[i + 2].kind == "string":
                            values, j = [], i + 2
                            while j < len(tokens) and tokens[j].kind == "string":
                                values.append(tokens[j].value)
                                if j + 1 >= len(tokens) or tokens[j + 1].value != "|":
                                    break
                                j += 2
                            if tokens[i].line <= expression[0].line:
                                unions.append((tokens[i].line, values))
                    options.append(max(unions, default=(0, []))[1])
                if names and all(options):
                    for values in itertools.product(*options):
                        expanded = template
                        for name, value in zip(names, values):
                            expanded = expanded.replace("${" + name + "}", value)
                        methods.append(expanded)
            if not methods and rel in HELPERS and call.signature == "connection.request(method , params)":
                bridge_counts[(rel, call.signature)] += 1
                classified.append({"source": f"{rel}:{call.line}", "expression": call.signature,
                                   "reason": "settings helper forwarding; concrete call producers scanned"})
                continue
            if not methods:
                errors.append(f"{rel}:{call.line}: unclassified dynamic call {call.signature}")
                continue
            for method in methods:
                if rel == D and call.callee == "request":
                    method = "delivery." + method
                sites.append({"method": method, "transport": transport, "source": f"{rel}:{call.line}",
                              "payload_fields": fields(payload),
                              "spreads": [text(payload[i + 1:i + 2]) for i, t in enumerate(payload) if t.value == "..."],
                              "expression": call.signature})
        # Outbox producers carry concrete methods that dynamic dispatch must not hide.
        for i in range(len(tokens) - 2) if rel in (W, M) else ():
            if tokens[i].value == "method" and tokens[i + 1].value == ":" and tokens[i + 2].kind == "string":
                if i and tokens[i - 1].value in ("{", ","):
                    payload = []
                    for j in range(i + 3, len(tokens) - 2):
                        if tokens[j].value == "params" and tokens[j + 1].value == ":":
                            if tokens[j + 2].value == "{":
                                payload = tokens[j + 2:close(tokens, j + 2) + 1]
                            break
                        if tokens[j].value == "}":
                            break
                    sites.append({"method": tokens[i + 2].value, "transport": "rpc",
                                  "source": f"{rel}:{tokens[i].line}", "payload_fields": fields(payload),
                                  "spreads": [text(payload[j + 1:j + 2]) for j, t in enumerate(payload) if t.value == "..."],
                                  "expression": "outbox method producer"})
    for rel, entries in BRIDGES.items():
        if not (root / rel).is_file():
            continue
        for signature, (expected, _) in entries.items():
            actual = bridge_counts[(rel, signature)]
            if actual != expected:
                errors.append(f"{rel}: audited dynamic bridge count {actual}, expected {expected}: {signature}")
    for (rel, signature), (expected, _) in LOCAL_CALLS.items():
        if (root / rel).is_file():
            actual = local_counts[(rel, signature)]
            if actual != expected:
                errors.append(f"{rel}: audited local call count {actual}, expected {expected}: {signature}")
    for rel in HELPERS:
        if not (root / rel).is_file():
            continue
        count = bridge_counts[(rel, "connection.request(method , params)")]
        if count != 1:
            errors.append(f"{rel}: settings forwarding count {count}, expected 1")
    return sites, classified, errors, payload_sources


def check(root, binary=None):
    enum_sources = {}
    for path, names in (
        ("crates/client/src/api.rs", ("ReadRequest", "Action")),
        ("crates/protocol/src/delivery.rs", ("DeliveryRead", "DeliveryAction")),
        ("crates/protocol/src/workspace.rs", ("WorkspaceCommand", "InputTarget", "InputControl")),
    ):
        tokens = lex((root / path).read_text())
        enum_sources.update({name: (path, tokens) for name in names})
    native = {name: enum(tokens, name) for name, (_, tokens) in enum_sources.items()}
    source_catalog = source_capabilities(lex((root / "apps/cli/src/client_commands/catalog.rs").read_text()))
    source_catalog["local_lifecycle"] = lifecycle(lex((root / "apps/cli/src/client_commands.rs").read_text()))
    catalog = source_catalog
    if binary:
        completed = subprocess.run([str(binary), "client", "capabilities"], text=True, capture_output=True, check=True)
        catalog = json.loads(completed.stdout)
    errors = []
    if catalog.get("schema_version") != 1:
        errors.append("capabilities: unsupported or missing schema_version")
    operations = catalog.get("operations", [])
    for name, kind in (("ReadRequest", "read"), ("Action", "write")):
        actual = {op["native_variant"]: op for op in operations if op.get("kind") == kind}
        for variant, description in native[name].items():
            source = f"{enum_sources[name][0]}:{description['line']}"
            if variant not in actual:
                errors.append(f"{source}: {name}::{variant} has no named CLI operation")
            elif actual[variant].get("tag") != description["tag"]:
                errors.append(f"{source}: {name}::{variant} CLI tag differs from native serde tag")
        for variant in actual.keys() - native[name].keys():
            errors.append(f"capabilities: unknown {name}::{variant}")
    for name in ("DeliveryRead", "DeliveryAction", "WorkspaceCommand", "InputTarget", "InputControl"):
        path, tokens = enum_sources[name]
        expected_discriminator = discriminator(tokens, name)
        if catalog.get("nested", {}).get(name, {}).get("tag") != expected_discriminator:
            errors.append(f"capabilities: {name} discriminator differs from native serde tag {expected_discriminator}")
        entries = catalog.get("nested", {}).get(name, {}).get("variants", [])
        actual = {entry["native_variant"]: entry for entry in entries}
        for variant, description in native[name].items():
            if variant not in actual or actual[variant].get("tag") != description["tag"]:
                errors.append(f"{path}:{description['line']}: missing/mismatched CLI nested {name}::{variant}")
        for variant in actual.keys() - native[name].keys():
            errors.append(f"capabilities: unknown nested {name}::{variant}")
    for op in operations:
        if not op.get("command", "").startswith(("client read ", "client write ")) or not op.get("core_methods"):
            errors.append(f"capabilities: invalid named CLI metadata {op}")
        if "call" in op.get("command", "").split():
            errors.append("raw Call cannot establish parity")
    if binary and catalog.get("operations") != source_catalog.get("operations"):
        # Examples and optional metadata may appear only in the live document.
        keys = ("kind", "native_variant", "command", "tag", "transport", "core_methods")
        normalize = lambda ops: sorted((tuple(json.dumps(op.get(key), sort_keys=True) for key in keys) for op in ops))
        if normalize(operations) != normalize(source_catalog["operations"]):
            errors.append("capabilities: binary operation metadata differs from current catalog source")
    mappings = json.loads((root / "packages/docs/cli-coverage.json").read_text())
    additional = mappings["additional_coverage"]
    coverage = {}
    for op in operations:
        for method in op["core_methods"]:
            coverage.setdefault(method, []).append(op["command"])
    for name in ("DeliveryRead", "DeliveryAction"):
        kind = "read" if name == "DeliveryRead" else "write"
        for entry in catalog.get("nested", {}).get(name, {}).get("variants", []):
            coverage["delivery." + entry["tag"]] = [f"client {kind} delivery --input op={entry['tag']}"]
    for mapping in additional:
        command = mapping["command"]
        if "call" in command.split():
            errors.append(f"mapping {command}: raw Call cannot establish parity")
            continue
        valid = True
        for evidence in mapping["evidence"]:
            path = root / evidence["path"]
            if not path.is_file() or evidence["contains"] not in path.read_text():
                errors.append(f"{evidence['path']}: stale evidence for {command}: {evidence['contains']}")
                valid = False
        if valid:
            for method in mapping["core_methods"]:
                coverage.setdefault(method, []).append(command)
    for local in catalog.get("local_lifecycle", []):
        for method in local.get("core_methods", []):
            if local.get("command") not in coverage.get(method, []):
                errors.append(f"capabilities: local lifecycle {local.get('command')} / {method} has no evidenced named CLI mapping")
    if catalog.get("local_lifecycle") != source_catalog["local_lifecycle"]:
        # The live document may include authority/native_variant annotations.
        normalize = lambda rows: [{k: row.get(k) for k in ("command", "core_methods")} for row in rows]
        if normalize(catalog.get("local_lifecycle", [])) != normalize(source_catalog["local_lifecycle"]):
            errors.append("capabilities: binary local_lifecycle differs from current source")
    sites, classified, web_errors, payload_sources = web_inventory(root)
    errors.extend(web_errors)
    transport = mappings["transport_only"]
    for site in sites:
        method = site["method"]
        if method in coverage:
            site["commands"] = coverage[method]
        elif method in transport and site["source"].rsplit(":", 1)[0] in transport[method]["sources"]:
            site["classification"] = transport[method]["reason"]
        else:
            errors.append(f"{site['source']}: Web {method} has no named CLI coverage")
    return {"schema_version": 1, "mode": "binary" if binary else "source", "passed": not errors,
            "native": native, "operations": operations, "web_calls": sites,
            "local_lifecycle": catalog.get("local_lifecycle", []),
            "signing_payload_sources": payload_sources,
            "classified_calls": classified, "errors": errors,
            "limitations": ["Lexical source inventory, not TypeScript type checking or full data-flow analysis.",
                            "Audited dynamic forwarding expressions fail when their expression/count changes.",
                            "Observed payload keys and spreads include signing fields but do not prove payload completeness.",
                            "Method/variant coverage does not prove behavior, authorization, reconciliation or file integrity."]}


def self_test():
    # Real scanner/checker functions see synthetic files and mutated call/enum data.
    source = '''// request("comment")\nconst prose = "rpc('prose')";
      const settle = (choice: "reply" | "dismiss") => null;
      connection.rpc("known", {nested: fn(1, {x: 2}), attestation});
      connection.rpc(`${choice}_unanswered`, {});'''
    with tempfile.TemporaryDirectory(prefix="cli-parity-") as temporary:
        root = Path(temporary)
        target = root / "packages/client/src/synthetic.ts"
        target.parent.mkdir(parents=True)
        (root / "apps/app/src").mkdir(parents=True)
        for path in ("crates/client/src", "crates/protocol/src", "apps/cli/src/client_commands", "packages/docs"):
            (root / path).mkdir(parents=True, exist_ok=True)
        api_source = '''#[serde(tag = "type", rename_all = "snake_case")]
          pub enum ReadRequest { Existing, }
          #[serde(tag = "type", rename_all = "snake_case")]
          pub enum Action { Write, }'''
        delivery_source = '''#[serde(tag = "op", rename_all = "snake_case")]
          pub enum DeliveryRead { Status, }
          #[serde(tag = "op", rename_all = "snake_case")]
          pub enum DeliveryAction<Flush = DeliveryFlushIntent> { Configure, }'''
        protocol_source = '''#[serde(tag = "kind", rename_all = "snake_case")]
          pub enum WorkspaceCommand { CreateTask, }
          #[serde(tag = "kind", rename_all = "snake_case")]
          pub enum InputTarget { Task, }
          #[serde(tag = "kind", rename_all = "snake_case")]
          pub enum InputControl { Release, }'''
        catalog_source = '''operation_catalog!(ReadCommand, ReadRequest, "read";
          Existing, ReadRequest::Existing, "existing", "existing", "rpc", ["known", "reply_unanswered", "dismiss_unanswered"], "Read", "{}";);
          operation_catalog!(WriteCommand, Action, "write";
          Write, Action::Write, "write", "write", "rpc", ["write"], "Write", "{}";);'''
        for native, variant, tag, key in (("DeliveryRead", "Status", "status", "op"),
                                          ("DeliveryAction", "Configure", "configure", "op"),
                                          ("WorkspaceCommand", "CreateTask", "create_task", "kind"),
                                          ("InputTarget", "Task", "task", "kind"),
                                          ("InputControl", "Release", "release", "kind")):
            catalog_source += f'\nnested_catalog!(check, metadata, {native}, "{key}"; {native}::{variant} => "{variant}", "{tag}", "{{}}";);'
        api_file = root / "crates/client/src/api.rs"
        api_file.write_text(api_source)
        delivery_file = root / "crates/protocol/src/delivery.rs"
        delivery_file.write_text(delivery_source)
        protocol_file = root / "crates/protocol/src/workspace.rs"
        protocol_file.write_text(protocol_source)
        (root / "apps/cli/src/client_commands/catalog.rs").write_text(catalog_source)
        (root / "apps/cli/src/client_commands.rs").write_text('json!({"local_lifecycle": []})')
        (root / "packages/docs/cli-coverage.json").write_text(json.dumps({"additional_coverage": [], "transport_only": {}}))
        target.write_text(source)
        local_file = root / "apps/app/src/lib/delivery-reservations.ts"
        local_file.parent.mkdir(parents=True)
        local_source = '''import type { ReserveDeliveryIntent } from '@vibeface/client/delivery/types.ts'

export function webDeliveryReservations(locks: Pick<LockManager, 'request'> | undefined): ReserveDeliveryIntent {
  return async (key, reserve) => {
    if (!locks) throw new Error('Unavailable')
    // Return callback failures as rejected promises so the lock manager releases ownership.
    return locks.request(`vibeface-delivery-intent:${key}`, { mode: 'exclusive' }, async () => reserve())
  }
}
'''
        local_file.write_text(local_source)
        baseline = check(root)
        assert baseline["passed"], baseline["errors"]
        sites = baseline["web_calls"]
        assert {s["method"] for s in sites} == {"known", "reply_unanswered", "dismiss_unanswered"}
        assert len(sites) == 3
        assert len(baseline["classified_calls"]) == 1
        assert baseline["classified_calls"][0]["source"] == "apps/app/src/lib/delivery-reservations.ts:7"
        for callee in ("connection.rpc", "connection.request"):
            local_file.write_text(local_source + f'\n{callee}(computed(), {{}});')
            changed = check(root)
            assert not changed["passed"] and any(
                f"delivery-reservations.ts:11: unclassified dynamic call {callee}(" in error
                for error in changed["errors"])
        local_call = 'locks.request(`vibeface-delivery-intent:${key}`, { mode: "exclusive" }, async () => reserve());'
        local_file.write_text(local_source + '\n' + local_call)
        changed = check(root)
        assert not changed["passed"] and any("audited local call count 2, expected 1" in error for error in changed["errors"])
        for changed_source in (local_source.replace("locks.request(", "connection.request("),
                               local_source.replace("'exclusive'", "'shared'")):
            local_file.write_text(changed_source)
            changed = check(root)
            assert not changed["passed"] and any("delivery-reservations.ts:7: unclassified dynamic call" in error for error in changed["errors"])
        local_file.write_text(local_source)
        target.write_text(source + '\n' + local_call)
        changed = check(root)
        assert not changed["passed"] and any("synthetic.ts:6: unclassified dynamic call locks.request(" in error for error in changed["errors"])
        target.write_text(source)
        assert "attestation" in sites[0]["payload_fields"]
        structured = 'signedWrite("chat", { method: "known", params: { nested: fn(1, {x: 2}), attestation } });'
        target.write_text(source + '\n' + structured)
        changed = check(root)
        assert changed["passed"], changed["errors"]
        projected, = [site for site in changed["web_calls"] if site["expression"].startswith('signedWrite(')]
        assert projected["method"] == "known" and projected["transport"] == "rpc"
        assert projected["payload_fields"] == ["attestation", "nested"]
        target.write_text(source + '\n' + structured.replace('"known"', '"new_operation"'))
        changed = check(root)
        assert not changed["passed"] and any("synthetic.ts:6: Web new_operation" in error for error in changed["errors"])
        for operation in ('{ method: computed(), params: {} }',
                          '{ method: "known", method: "write", params: {} }',
                          '{ method: "known", ...operation, params: {} }',
                          '{ [key]: "known", method: "known", params: {} }',
                          '{ method: "known" }'):
            target.write_text(source + '\nsignedWrite("chat", ' + operation + ');')
            changed = check(root)
            assert not changed["passed"] and any("synthetic.ts:6: unclassified dynamic call signedWrite(" in error for error in changed["errors"])
        target.write_text(source)
        forwarding_file = root / W
        forwarding_file.parent.mkdir(parents=True)
        forwarding = '''rpc(item.method, item.params);
rpc(item.method, item.params);
rpc(method, params);
rpc(method, params);
connection.rpc(method, params);
notifier.request();'''
        forwarding_file.write_text(forwarding)
        assert check(root)["passed"]
        for changed_forwarding in (forwarding.replace('rpc(item.method', 'connection.rpc(item.method', 1),
                                   forwarding + '\nrpc(method, params);',
                                   forwarding.replace('connection.rpc(method, params);', '')):
            forwarding_file.write_text(changed_forwarding)
            changed = check(root)
            assert not changed["passed"] and any("audited dynamic bridge count" in error for error in changed["errors"])
        forwarding_file.write_text(forwarding)
        target.write_text(source + '\nrpc(item.method, item.params);')
        changed = check(root)
        assert not changed["passed"] and any("synthetic.ts:6: unclassified dynamic call rpc(" in error for error in changed["errors"])
        forwarding_file.unlink()
        target.write_text(source)
        target.write_text(source + '\nconnection.rpc("new_operation", {});')
        changed = check(root)
        assert not changed["passed"] and any("synthetic.ts:6: Web new_operation" in error for error in changed["errors"])
        target.write_text(source + '\nconnection.rpc(computed(), {});')
        _, _, errors, _ = web_inventory(root)
        assert any("unclassified dynamic call" in error and "synthetic.ts:6" in error for error in errors)
        target.write_text(source)
        api_file.write_text(api_source.replace("Existing,", "Existing, Added { id: String },"))
        changed = check(root)
        assert not changed["passed"] and any("ReadRequest::Added has no named CLI" in error for error in changed["errors"])
        api_file.write_text(api_source)
        delivery_file.write_text(delivery_source.replace("Configure,", "Configure, Added,"))
        changed = check(root)
        assert not changed["passed"] and any(
            error.startswith("crates/protocol/src/delivery.rs:4:")
            and "DeliveryAction::Added" in error for error in changed["errors"]
        )
        delivery_file.write_text(delivery_source)
        protocol_file.write_text(protocol_source.replace("Task,", "Task, NewTarget,"))
        changed = check(root)
        assert not changed["passed"] and any("InputTarget::NewTarget" in error for error in changed["errors"])
        protocol_file.write_text(protocol_source)
        assert check(root)["passed"]
    print("CLI parity self-test passed: structured writes and guarded forwarding are inventoried; unknown/computed/ambiguous writes, bridge receiver/path/count changes, browser locks, new native and nested operations fail with source locations")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--binary", type=Path, help="run PATH client capabilities without authentication")
    parser.add_argument("--output", type=Path, help="write the complete JSON report")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    try:
        report = check(args.root.resolve(), args.binary.resolve() if args.binary else None)
    except (ValueError, OSError, KeyError, IndexError, StopIteration, subprocess.SubprocessError) as error:
        print(f"CLI parity scanner failed: {error}", file=sys.stderr)
        return 2
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(f"CLI parity {'passed' if report['passed'] else 'failed'}: {len(report['operations'])} native commands, {len(report['web_calls'])} Web call records, {len(report['errors'])} gaps")
    for error in report["errors"]:
        print(error)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
