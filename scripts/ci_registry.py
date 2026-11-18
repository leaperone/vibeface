#!/usr/bin/env python3
"""Shared CI and preflight registry loaded with Python's standard library."""
from __future__ import annotations

import argparse
import fnmatch
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
REGISTRY_PATH = ROOT / "scripts" / "ci-registry.toml"
PREFLIGHT_PATH = ROOT / ".preflight.toml"
BEGIN = "# BEGIN CI REGISTRY GENERATED DOMAINS\n"
END = "# END CI REGISTRY GENERATED DOMAINS\n"


def _as_strings(value, label):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise ValueError(f"{label} must be a non-empty string list")
    return tuple(value)


def load_registry(path: Path = REGISTRY_PATH):
    try:
        data = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ValueError(f"Unable to load CI registry {path}: {exc}") from exc
    if data.get("schema_version") != 1:
        raise ValueError("Unsupported CI registry schema")
    paths = data.get("paths")
    if not isinstance(paths, dict):
        raise ValueError("Registry paths table is required")
    groups = {}
    for group_id, spec in paths.items():
        if not isinstance(spec, dict):
            raise ValueError(f"Path group {group_id} must be a table")
        direct = list(_as_strings(spec.get("patterns", []), f"Path group {group_id}.patterns")) if "patterns" in spec else []
        exact = _as_strings(spec.get("exact", []), f"Path group {group_id}.exact") if "exact" in spec else ()
        trees = _as_strings(spec.get("trees", []), f"Path group {group_id}.trees") if "trees" in spec else ()
        direct.extend(exact)
        direct.extend(tree.rstrip("/") + "/*" for tree in trees)
        include = tuple(spec.get("include", ()))
        if any(not isinstance(item, str) or item not in paths for item in include):
            raise ValueError(f"Path group {group_id} has an unknown include")
        groups[group_id] = (tuple(direct), include)
    resolved = {}
    visiting = set()
    def resolve(group_id):
        if group_id in resolved:
            return resolved[group_id]
        if group_id in visiting:
            raise ValueError(f"Path group cycle at {group_id}")
        visiting.add(group_id)
        direct, include = groups[group_id]
        values = list(direct)
        for child in include:
            values.extend(resolve(child))
        visiting.remove(group_id)
        deduped = tuple(dict.fromkeys(values))
        resolved[group_id] = deduped
        return deduped
    for group_id in groups:
        resolve(group_id)

    def entries(name):
        raw = data.get(name, [])
        if not isinstance(raw, list):
            raise ValueError(f"Registry {name} must be an array")
        seen = set()
        result = []
        for item in raw:
            if not isinstance(item, dict) or not isinstance(item.get("id"), str) or not item["id"]:
                raise ValueError(f"Registry {name} entries need an id")
            if item["id"] in seen:
                raise ValueError(f"Duplicate {name} id {item['id']}")
            seen.add(item["id"])
            result.append(item)
        return result

    def expand(item, label):
        values = []
        for group_id in item.get("paths", []):
            if group_id in resolved:
                values.extend(resolved[group_id])
            else:
                raise ValueError(f"{label} references unknown path group {group_id}")
        values.extend(_as_strings(item.get("exact", []), f"{label}.exact") if "exact" in item else ())
        return tuple(dict.fromkeys(values))

    primary = entries("primary")
    overlay = entries("overlay")
    process = entries("process")
    rust = entries("rust_target")
    preflight = entries("preflight")
    for item in primary + overlay:
        item["patterns"] = expand(item, item["id"])
    for item in process:
        if "patterns" in item:
            if "paths" in item:
                raise ValueError(f"{item['id']} cannot mix process patterns and path groups")
            item["patterns"] = _as_strings(item["patterns"], f"process {item['id']}.patterns")
        else:
            item["patterns"] = expand(item, item["id"])
    for item in rust:
        if not isinstance(item.get("package"), str) or not isinstance(item.get("dependents"), list):
            raise ValueError(f"Invalid Rust target {item['id']}")
        if any(not isinstance(dep, str) for dep in item["dependents"]):
            raise ValueError(f"Invalid Rust dependents for {item['id']}")
    rust_ids = {item["id"] for item in rust}
    if len(rust_ids) != len(rust) or any(dep not in rust_ids for item in rust for dep in item["dependents"]):
        raise ValueError("Rust target dependency references are invalid")
    for item in preflight:
        trigger = list(_as_strings(item.get("paths", []), f"preflight {item['id']}.paths") if "paths" in item else ())
        for group_id in item.get("groups", []):
            if group_id not in resolved:
                raise ValueError(f"preflight {item['id']} references unknown path group {group_id}")
            trigger.extend(resolved[group_id])
        if not trigger:
            raise ValueError(f"preflight {item['id']} has no trigger paths")
        item["trigger_paths"] = tuple(dict.fromkeys(trigger))
        if not isinstance(item.get("name"), str) or not isinstance(item.get("command"), str):
            raise ValueError(f"Invalid preflight entry {item['id']}")
        if "heavy" in item and not isinstance(item["heavy"], bool):
            raise ValueError(f"Invalid preflight heavy flag {item['id']}")
    # Exact rules at one priority must be unique. Prefix and regex rules may overlap by design.
    exact_seen = set()
    for item in primary + overlay:
        for value in item.get("exact", []):
            if value in exact_seen:
                raise ValueError(f"Duplicate same-priority exact route {value}")
            exact_seen.add(value)
    for item in process:
        if "ordinary" in item and not isinstance(item["ordinary"], bool):
            raise ValueError(f"Invalid ordinary flag for process {item['id']}")
    return {"data": data, "paths": resolved, "primary": primary, "overlay": overlay,
            "process": process, "rust": rust, "preflight": preflight}


def fnmatch_patterns(registry, group):
    return registry["paths"].get(group, ())


def render_preflight(registry):
    lines = [BEGIN]
    for item in registry["preflight"]:
        paths = json.dumps(list(item["trigger_paths"]), ensure_ascii=False)
        lines.extend(["[[domain_check]]\n", f"name = {json.dumps(item['name'], ensure_ascii=False)}\n",
                      f"trigger_paths = {paths}\n", f"command = {json.dumps(item['command'], ensure_ascii=False)}\n"])
        if item.get("heavy"):
            if item.get("comment"):
                lines.append(f"heavy = true # {item['comment']}\n")
            else:
                lines.append("heavy = true\n")
        if item.get("ci_check") is not None:
            lines.append(f"ci_check = {json.dumps(item['ci_check'], ensure_ascii=False)}\n")
        lines.append("\n")
    lines.append(END)
    return "".join(lines)


def check_preflight(registry):
    try:
        current = PREFLIGHT_PATH.read_text()
    except OSError as exc:
        raise ValueError(f"Unable to read {PREFLIGHT_PATH}: {exc}") from exc
    if BEGIN in current and END in current:
        start = current.index(BEGIN)
        end = current.index(END, start) + len(END)
        expected = current[:start] + render_preflight(registry) + current[end:]
    else:
        # First migration accepts the existing domain block only when it is byte-equivalent
        # after replacing the first through last domain entry.
        marker_start = current.find("[[domain_check]]")
        suffix_start = current.find("[review]", marker_start)
        if marker_start < 0 or suffix_start < 0:
            raise ValueError(".preflight.toml has no domain_check block")
        expected = current[:marker_start] + render_preflight(registry) + current[suffix_start:]
    if current != expected:
        raise ValueError(".preflight.toml CI registry domain block drifted")


def write_preflight(registry):
    current = PREFLIGHT_PATH.read_text()
    generated = render_preflight(registry)
    if BEGIN in current and END in current:
        start = current.index(BEGIN)
        end = current.index(END, start) + len(END)
        updated = current[:start] + generated + current[end:]
    else:
        marker_start = current.find("[[domain_check]]")
        suffix_start = current.find("[review]", marker_start)
        if marker_start < 0 or suffix_start < 0:
            raise ValueError(".preflight.toml has no domain_check block")
        updated = current[:marker_start] + generated + current[suffix_start:]
    PREFLIGHT_PATH.write_text(updated)


def self_test(registry):
    assert registry["primary"] and registry["overlay"] and registry["process"]
    assert registry["paths"]["delivery_probe_protocol"] == ("crates/protocol/src/*.rs",)
    assert any(item["id"] == "delivery-acceptance" for item in registry["overlay"])
    assert [item["id"] for item in registry["rust"]] == ["protocol", "bridge", "secure", "client", "node", "control", "cli", "secure-ffi", "delivery"]
    check_preflight(registry)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--check-preflight", action="store_true")
    parser.add_argument("--write-preflight", action="store_true")
    args = parser.parse_args(argv)
    if args.check_preflight and args.write_preflight:
        parser.error("choose one preflight action")
    try:
        registry = load_registry()
        if args.self_test:
            self_test(registry)
            print("CI registry self-test passed")
        elif args.check_preflight:
            check_preflight(registry)
            print("Preflight registry is up to date")
        elif args.write_preflight:
            write_preflight(registry)
            check_preflight(registry)
            print("Generated preflight domains")
        else:
            parser.error("select an action")
    except (OSError, ValueError) as exc:
        print(f"CI registry error: {exc}", file=sys.stderr)
        return 1
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
