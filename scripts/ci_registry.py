#!/usr/bin/env python3
"""Shared CI and preflight registry loaded with Python's standard library."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import re
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
REGISTRY_PATH = ROOT / "scripts" / "ci-registry.toml"
PREFLIGHT_PATH = ROOT / ".preflight.toml"
BEGIN = "# BEGIN CI REGISTRY GENERATED DOMAINS\n"
END = "# END CI REGISTRY GENERATED DOMAINS\n"
JOB_SCOPES = frozenset({
    "full", "docs", "release-tools", "web", "web-tests", "www", "mobile",
    "desktop", "delivery-acceptance",
})


def _as_strings(value, label):
    if not isinstance(value, list) or any(not isinstance(item, str) or not item for item in value):
        raise ValueError(f"{label} must be a list of nonempty strings")
    return tuple(value)


def load_registry(path: Path = REGISTRY_PATH):
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as exc:
        raise ValueError(f"Unable to load CI registry {path}: {exc}") from exc
    if type(data.get("schema_version")) is not int or data["schema_version"] != 1:
        raise ValueError("Unsupported CI registry schema")
    paths = data.get("paths")
    if not isinstance(paths, dict):
        raise ValueError("Registry paths table is required")
    groups = {}
    for group_id, spec in paths.items():
        if not isinstance(spec, dict) or spec.keys() - {"patterns", "exact", "trees", "include"}:
            raise ValueError(f"Invalid path group {group_id}")
        fields = f"Path group {group_id}"
        direct = list(_as_strings(spec.get("patterns", []), f"{fields}.patterns"))
        exact = _as_strings(spec.get("exact", []), f"{fields}.exact")
        trees = _as_strings(spec.get("trees", []), f"{fields}.trees")
        direct.extend(exact)
        direct.extend(tree.rstrip("/") + "/**" for tree in trees)
        include = _as_strings(spec.get("include", []), f"{fields}.include")
        if any(item not in paths for item in include):
            raise ValueError(f"Path group {group_id} has an unknown include")
        if not direct and not include:
            raise ValueError(f"Path group {group_id} is empty")
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
        if not isinstance(raw, list) or not raw:
            raise ValueError(f"Registry {name} must be a nonempty array")
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
        for group_id in _as_strings(item.get("paths", []), f"{label}.paths"):
            if group_id in resolved:
                values.extend(resolved[group_id])
            else:
                raise ValueError(f"{label} references unknown path group {group_id}")
        return tuple(dict.fromkeys(values))

    primary = entries("primary")
    overlay = entries("overlay")
    process = entries("process")
    rust = entries("rust_target")
    preflight = entries("preflight")
    rust_ids = {item["id"] for item in rust}
    scopes = JOB_SCOPES | rust_ids
    if rust_ids & JOB_SCOPES:
        raise ValueError("Rust target IDs may not shadow job scopes")
    if any(item.keys() & {"exact", "regex", "prefix", "suffix"} for item in overlay):
        raise ValueError("Overlays must select path groups")
    for item in primary + overlay:
        label = f"Route {item['id']}"
        route_scopes = _as_strings(item.get("scopes", []), f"{label}.scopes")
        if not route_scopes or set(route_scopes) - scopes:
            raise ValueError(f"{label} has no scopes or an unknown scope")
        item["scopes"] = route_scopes
        selectors = item.keys() & {"paths", "exact", "regex", "prefix"}
        if len(selectors) != 1 or "suffix" in item and "prefix" not in selectors:
            raise ValueError(f"{label} must define one selector")
        if "paths" in selectors:
            item["patterns"] = expand(item, label)
            if not item["patterns"]:
                raise ValueError(f"{label} has no path patterns")
        elif "exact" in selectors:
            item["exact"] = _as_strings(item["exact"], f"{label}.exact")
            if not item["exact"]:
                raise ValueError(f"{label} has no exact paths")
        elif "regex" in selectors:
            if not isinstance(item["regex"], str) or not item["regex"]:
                raise ValueError(f"{label} requires a regex")
            try:
                item["regex"] = re.compile(item["regex"])
            except re.error as exc:
                raise ValueError(f"{label} has an invalid regex: {exc}") from exc
        else:
            if not isinstance(item["prefix"], str) or not item["prefix"]:
                raise ValueError(f"{label} requires a path prefix")
            item["suffix"] = _as_strings(item.get("suffix", []), f"{label}.suffix")
    for item in process:
        if "patterns" in item:
            if "paths" in item:
                raise ValueError(f"{item['id']} cannot mix process patterns and path groups")
            item["patterns"] = _as_strings(item["patterns"], f"process {item['id']}.patterns")
        else:
            item["patterns"] = expand(item, item["id"])
        if not item["patterns"]:
            raise ValueError(f"Process {item['id']} has no path patterns")
    for item in rust:
        if not isinstance(item.get("package"), str) or not item["package"]:
            raise ValueError(f"Invalid Rust target {item['id']}")
        item["dependents"] = _as_strings(item.get("dependents"), f"Rust target {item['id']}.dependents")
    if len({item["package"] for item in rust}) != len(rust) or any(
        dep not in rust_ids for item in rust for dep in item["dependents"]
    ):
        raise ValueError("Rust target dependency references are invalid")
    for item in preflight:
        trigger = list(_as_strings(item.get("paths", []), f"preflight {item['id']}.paths"))
        for group_id in _as_strings(item.get("groups", []), f"preflight {item['id']}.groups"):
            if group_id not in resolved:
                raise ValueError(f"preflight {item['id']} references unknown path group {group_id}")
            trigger.extend(resolved[group_id])
        if not trigger:
            raise ValueError(f"preflight {item['id']} has no trigger paths")
        item["trigger_paths"] = tuple(dict.fromkeys(trigger))
        if any(not isinstance(item.get(field), str) or not item[field] for field in ("name", "command")):
            raise ValueError(f"Invalid preflight entry {item['id']}")
        if "ci_check" in item and (not isinstance(item["ci_check"], str) or not item["ci_check"]):
            raise ValueError(f"Invalid preflight CI check {item['id']}")
        if "heavy" in item and not isinstance(item["heavy"], bool):
            raise ValueError(f"Invalid preflight heavy flag {item['id']}")
    # Exact rules at one priority must be unique. Prefix and regex rules may overlap by design.
    exact_seen = set()
    for item in primary:
        for value in item.get("exact", []):
            if value in exact_seen:
                raise ValueError(f"Duplicate same-priority exact route {value}")
            exact_seen.add(value)
    for item in process:
        if "ordinary" in item and not isinstance(item["ordinary"], bool):
            raise ValueError(f"Invalid ordinary flag for process {item['id']}")
    return {"scopes": scopes, "paths": resolved, "primary": primary, "overlay": overlay,
            "process": process, "rust": rust, "preflight": preflight}


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


def update_domains(current, registry):
    if current.count(BEGIN) != 1 or current.count(END) != 1:
        raise ValueError(".preflight.toml requires one generated domain block")
    start = current.index(BEGIN)
    end = current.index(END)
    if end < start:
        raise ValueError(".preflight.toml domain block markers are out of order")
    generated = render_preflight(registry)
    updated = current[:start] + generated + current[end + len(END):]
    if tomllib.loads(updated).get("domain_check") != tomllib.loads(generated)["domain_check"]:
        raise ValueError(".preflight.toml has domain checks outside the registry block")
    return updated


def check_preflight(registry, path=PREFLIGHT_PATH):
    current = path.read_bytes().decode("utf-8")
    if current != update_domains(current, registry):
        raise ValueError(".preflight.toml CI registry domain block drifted")


def write_preflight(registry):
    current = PREFLIGHT_PATH.read_bytes().decode("utf-8")
    PREFLIGHT_PATH.write_bytes(update_domains(current, registry).encode("utf-8"))


def self_test(registry):
    assert registry["primary"] and registry["overlay"] and registry["process"]
    assert registry["paths"]["delivery_probe_protocol"] == ("crates/protocol/src/**",)
    assert any(item["id"] == "delivery-acceptance" for item in registry["overlay"])
    assert [item["id"] for item in registry["rust"]] == ["protocol", "bridge", "secure", "client", "node", "control", "cli", "secure-ffi", "delivery"]
    check_preflight(registry)
    with tempfile.TemporaryDirectory(prefix="ci-registry-") as directory:
        bad_registry = Path(directory) / "registry.toml"
        actual = REGISTRY_PATH.read_text()
        for original, invalid in (
            ('scopes = ["delivery"]', 'scopes = ["delivrey"]'),
            ('paths = ["delivery", "delivery_probe_protocol"]', 'paths = ["not_a_group"]'),
        ):
            assert original in actual
            bad_registry.write_text(actual.replace(original, invalid, 1))
            try:
                load_registry(bad_registry)
            except ValueError:
                pass
            else:
                raise AssertionError(f"Invalid registry accepted {invalid}")
        bad_preflight = Path(directory) / ".preflight.toml"
        config = PREFLIGHT_PATH.read_text()
        for bad in (
            config + "\n[",
            config + '\n[[domain_check]]\nname = "unregistered"\n',
            config.replace(BEGIN, ""),
            config.replace("[[domain_check]]", "# [[domain_check]]", 1),
        ):
            bad_preflight.write_text(bad)
            try:
                check_preflight(registry, bad_preflight)
            except ValueError:
                pass
            else:
                raise AssertionError("Malformed or unregistered preflight accepted")


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
