#!/usr/bin/env python3
"""Fresh protected paid Node acceptance, called by verify-node with final artifacts."""

import argparse
import base64
import copy
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import platform
import runpy
import re
import signal
import sqlite3
import stat
import subprocess
import sys
import time
import traceback
import urllib.parse
import urllib.request
import uuid


REPOSITORY = Path(__file__).resolve().parents[1]
ARTIFACTS = REPOSITORY / ".artifacts"
CAP = "100000000000000000"
POLICY = {"watchdog_seconds": 60, "inclusion_seconds": 60, "finality_seconds": 10,
          "clock_skew_seconds": 5, "max_fee_per_gas": "3000000000", "priority_fee_per_gas": "1000000000"}
PROFILE_FIELDS = ("crypto_suite", "max_windows", "max_window_bytes", "max_total_bytes", "max_guest_cycles", "max_prover_memory_bytes")
NOT_CHECKED = ["old fee5 failure remains its original baseline", "same bytes or maximum size as old fee5 case",
               "strict or Open relations", "Web or Android rendered UI", "public network or real assets",
               "absolute finality or real-world economics", "cross-container chain/order resume",
               "ordinary free discovery and charity transfer regression",
               "forced reorg, nonce-consuming revert, near-cutoff, keeper or token-failure matrix"]


class CheckFailure(RuntimeError):
    pass


def need(value, code):
    if not value:
        raise CheckFailure(code)


def exact(value, fields, code):
    need(isinstance(value, dict) and set(value) == set(fields), code)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_regular(path, limit, *, private=False):
    info = path.lstat()
    need(stat.S_ISREG(info.st_mode) and 0 < info.st_size <= limit, "bounded_regular_file_required")
    if private:
        need(info.st_uid == os.getuid() and not info.st_mode & 0o077, "owner_only_file_required")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), "rb") as stream:
        data = stream.read(limit + 1)
    need(0 < len(data) <= limit, "bounded_file_read_required")
    return data


def file_digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for part in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(part)
    return value.hexdigest()


def durable(path, data):
    need(path.parent.resolve().is_relative_to(ARTIFACTS.resolve()), "write_outside_task_artifacts")
    temporary = path.with_name("." + path.name + "." + uuid.uuid4().hex)
    with os.fdopen(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600), "wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    descriptor = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def save_json(path, value):
    durable(path, (json.dumps(value, indent=2, sort_keys=True) + "\n").encode())


def validate_config(value):
    exact(value, {"schema_version", "source_commit", "paths", "sha256", "elf_sha256", "caps", "publication_policy"}, "exact_config_schema_required")
    need(type(value["schema_version"]) is int and value["schema_version"] == 1, "config_schema_version")
    need(isinstance(value["source_commit"], str) and re.fullmatch(r"[0-9a-f]{40}", value["source_commit"])
         and value["source_commit"] != "0" * 40, "final_product_commit_required")
    exact(value["paths"], {"repository", "binary", "core_binary", "crypto_module", "prover", "r0vm"}, "exact_path_schema_required")
    for item in value["paths"].values():
        need(isinstance(item, str) and Path(item).is_absolute() and "trash" not in item.lower()
             and ".." not in Path(item).parts, "absolute_safe_paths_required")
    paths = {name: Path(item) for name, item in value["paths"].items()}
    need(paths["repository"].resolve() == REPOSITORY, "current_source_repository_required")
    need(paths["binary"].name == "vibeface" and paths["core_binary"] == paths["binary"].with_name("vibeface-core"), "same_directory_cli_core_required")
    need(paths["prover"].name == "vibeface-delivery-prover" and paths["crypto_module"].name == "vibeface_secure.js", "explicit_host_and_crypto_required")
    exact(value["sha256"], {"binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile"}, "exact_hash_schema_required")
    for item in [*value["sha256"].values(), value["elf_sha256"]]:
        need(isinstance(item, str) and re.fullmatch(r"[0-9a-f]{64}", item) and item != "0" * 64, "explicit_final_artifact_hash_required")
    need(value["caps"] == {"deposit": "10", "max_service_amount": "5", "source_gas_cap_wei": CAP,
                          "provider_gas_cap_wei": CAP, "register_key_gas_cap_wei": CAP,
                          "native_funding_wei_each": str(10 ** 19)}, "explicit_test_caps_required")
    need(value["publication_policy"] == POLICY, "explicit_local_publication_policy_required")
    return value


def loopback(url):
    value = urllib.parse.urlsplit(url)
    need(value.scheme == "http" and value.hostname and ipaddress.ip_address(value.hostname).is_loopback
         and value.port and not value.username and not value.password and value.path in ("", "/")
         and not value.query and not value.fragment, "literal_loopback_rpc_required")
    return url


def extract_journal(receipt, expected_digest):
    need(0 < len(receipt) <= 16384 and re.fullmatch(r"[0-9a-f]{64}", expected_digest), "bounded_receipt_and_statement_digest_required")
    candidates = [receipt[index:index + 1504] for index in range(max(0, len(receipt) - 1504 + 1))
                  if digest(receipt[index:index + 1504]) == expected_digest]
    need(len(candidates) == 1, "unique_47_word_journal_required")
    return candidates[0]


def words(journal):
    need(len(journal) == 1504, "exact_47_word_statement_required")
    return [journal[index:index + 32] for index in range(0, 1504, 32)]


def wrong_journal(journal):
    fields = words(journal)
    relation = int.from_bytes(fields[1], "big")
    if relation in (2, 3):
        fields[1] = (5 - relation).to_bytes(32, "big")
    else:
        need(relation == 1, "expected_key_source_or_hold_relation")
        changed = bytearray(fields[4])
        changed[0] ^= 1
        need(any(changed), "alternate_profile_remains_nonzero")
        fields[4] = bytes(changed)
    return b"".join(fields)


def profile_id(profile):
    encoded = hashlib.sha256(b"vibeface-delivery-profile-v1").digest() + bytes(profile["guest_image_id"]) + bytes(profile["statement_schema_hash"])
    return digest(encoded + b"".join(profile[name].to_bytes(32, "big") for name in PROFILE_FIELDS))


_live_helpers = runpy.run_path(str(REPOSITORY / "scripts/run-delivery-live.py"))
resources = _live_helpers["resources"]
oom_delta = _live_helpers["oom_delta"]


def process_fact(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "birth": fields[19], "group": int(fields[2]), "state": fields[0]}
    except FileNotFoundError:
        return None


class OwnedProcesses:
    def __init__(self):
        self.facts = {}

    def capture(self):
        pending, seen = [os.getpid()], set()
        while pending:
            pid = pending.pop()
            if pid in seen:
                continue
            seen.add(pid)
            fact = process_fact(pid)
            if fact is None:
                continue
            if pid != os.getpid():
                self.facts[pid] = fact
            for task in Path(f"/proc/{pid}/task").glob("*"):
                try:
                    pending.extend(map(int, (task / "children").read_text().split()))
                except FileNotFoundError:
                    pass

    def alive(self):
        return [fact for fact in self.facts.values() if (current := process_fact(fact["pid"]))
                and current["birth"] == fact["birth"] and current["state"] != "Z"]

    def stop_all(self):
        self.capture()
        for action in (signal.SIGTERM, signal.SIGKILL):
            for fact in reversed(self.alive()):
                try:
                    os.kill(fact["pid"], action)
                except ProcessLookupError:
                    pass
            deadline = time.monotonic() + (8 if action == signal.SIGTERM else 2)
            while self.alive() and time.monotonic() < deadline:
                time.sleep(0.1)
        return [fact["pid"] for fact in self.alive()]


class Experiment:
    def __init__(self, config, helpers, run_directory):
        self.config = validate_config(config)
        self.helpers = helpers
        self.paths = {name: Path(value) for name, value in config["paths"].items()}
        self.repository = self.paths["repository"]
        self.run = Path(run_directory).resolve()
        need(self.run.is_relative_to(ARTIFACTS.resolve()) and not self.run.exists(), "fresh_run_directory_inside_task_artifacts_required")
        self.run.mkdir(mode=0o700, parents=True)
        self.environment = {**os.environ, "DELIVERY_CONTRACT_ARTIFACTS": str(self.run / "contracts")}
        self.public = self.run / "public"
        self.public.mkdir(mode=0o700)
        self.processes = OwnedProcesses()
        self.nodes, self.passwords, self.proofs, self.orders, self.receipt_seen = [], {}, {}, {}, {}
        self.deployment = None
        self.budget = None
        self.relay = None
        self.initial_resources = None
        self.started = time.monotonic()
        self.stage = "initializing"
        self.summary = {"schema_version": 1, "status": "RUNNING", "source_commit": config["source_commit"],
                        "run_id": self.run.name,
                        "scope": "fresh-two-order-protected-native-only", "parent_goal_status": "NOT_DETERMINED_BY_THIS_RUN",
                        "helper_source_sha256": file_digest(self.repository / "scripts/verify-node.py"),
                        "driver_source_sha256": file_digest(Path(__file__)), "artifact_sha256": config["sha256"], "node_proof_timeout_seconds": 7200,
                        "fees_each": {"delivery": "5", "reservation": "0", "bonus": "0"},
                        "deposit": "10", "publication_policy": POLICY, "not_checked": NOT_CHECKED,
                        "genuine_receipts": [], "public_files": {}, "orders": [], "checkpoints": {}}

    def public_json(self, name, value):
        path = self.public / name
        save_json(path, value)
        self.summary["public_files"][name] = file_digest(path)

    def progress(self, stage=None, **facts):
        if stage:
            self.stage = stage
        self.summary.update(facts)
        self.summary.update({"stage": self.stage, "wall_clock_unix_seconds": int(time.time()),
                             "elapsed_seconds": round(time.monotonic() - self.started, 3)})
        encoded = (json.dumps(self.summary, indent=2, sort_keys=True) + "\n").encode()
        need(len(encoded) <= 131072, "bounded_safe_progress_required")
        durable(self.public / "progress.json", encoded)
        print(json.dumps({"event": "protected_native_progress", "status": self.summary["status"], "stage": self.stage,
                          "run_id": self.run.name, "verified_genuine_receipts": len(self.proofs),
                          "source_commit": self.config["source_commit"]}, sort_keys=True), flush=True)

    def command(self, arguments, *, data=None, timeout=30, environment=None, success=True):
        result = subprocess.run(list(map(str, arguments)), input=data, capture_output=True, timeout=timeout,
                                env=self.environment if environment is None else environment, cwd=self.repository)
        self.processes.capture()
        need(len(result.stdout) <= 1024 * 1024 and len(result.stderr) <= 1024 * 1024, "bounded_child_output_required")
        if (result.returncode == 0) != success:
            failure = self.run / ("command-failure-" + uuid.uuid4().hex + ".log")
            durable(failure, result.stdout + b"\n" + result.stderr)
            raise CheckFailure("child_command_failed_private_log_retained")
        return result

    def wait(self, label, predicate, *, timeout=120):
        self.progress(label)
        until, checkpoint = time.monotonic() + timeout, 0
        while time.monotonic() < until:
            self.processes.capture()
            self.scan_receipts()
            value = predicate()
            if value:
                return value
            if time.monotonic() >= checkpoint:
                self.checkpoint(label)
                checkpoint = time.monotonic() + 30
            time.sleep(1)
        raise CheckFailure("stage_observer_timeout_" + label)

    def checkpoint(self, name):
        _, snapshot = resources()
        deltas = oom_delta(self.initial_resources, snapshot)
        need(not any(deltas.values()), "actual_cgroup_oom_during_experiment")
        anchor = self.rpc("eth_getBlockByNumber", ["latest", False]) if self.deployment else None
        value = {"wall_clock_unix_seconds": int(time.time()), "resources": snapshot, "oom_delta": deltas,
                 "original_deadlines": {key: item["deadlines"] for key, item in self.orders.items()}}
        if anchor:
            value["chain_anchor"] = {"number": int(anchor["number"], 16), "hash": anchor["hash"], "timestamp": int(anchor["timestamp"], 16)}
            value["deadline_remaining_seconds"] = {key: {field: deadline - int(anchor["timestamp"], 16)
                 for field, deadline in item["deadlines"].items()} for key, item in self.orders.items()}
        self.summary["checkpoints"][name] = value
        self.progress()

    def rpc(self, method, params):
        request = urllib.request.Request(loopback(self.deployment["rpc_url"]), json.dumps({"jsonrpc": "2.0", "id": 1,
            "method": method, "params": params}).encode(), {"Content-Type": "application/json"})
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(request, timeout=30) as response:
            encoded = response.read(1024 * 1024 + 1)
        need(len(encoded) <= 1024 * 1024, "bounded_loopback_rpc_response")
        result = json.loads(encoded)
        need(result.get("id") == 1 and "error" not in result, "successful_loopback_rpc_required")
        return result["result"]

    def confirmed(self, tx_hash):
        value = self.rpc("eth_getTransactionReceipt", [tx_hash])
        if value is None:
            return None
        need(value["status"] == "0x1", "task_funding_transaction_reverted")
        return value if int(self.rpc("eth_blockNumber", []), 16) >= int(value["blockNumber"], 16) + 1 else None

    def cli(self, node, arguments, *, password=None, timeout=30, success=True):
        command = [str(self.paths["binary"]), "wallet", "--data-dir", str(node.directory), *arguments]
        descriptor = None
        password_path = self.run / ("passphrase-" + uuid.uuid4().hex)
        try:
            if password is not None:
                descriptor = os.open(password_path, os.O_RDWR | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                os.write(descriptor, password.encode() + b"\n")
                os.lseek(descriptor, 0, os.SEEK_SET)
                command += ["--passphrase-fd", str(descriptor)]
            result = subprocess.run(command, capture_output=True, timeout=timeout,
                                    pass_fds=(() if descriptor is None else (descriptor,)))
        finally:
            if descriptor is not None:
                os.close(descriptor)
                password_path.unlink(missing_ok=True)
        if (result.returncode == 0) != success or len(result.stdout) > 65536 or len(result.stderr) > 65536:
            durable(self.run / ("wallet-failure-" + uuid.uuid4().hex + ".log"), result.stdout[:65536] + b"\n" + result.stderr[:65536])
            raise CheckFailure("local_owner_wallet_command_failed_private_log_retained")
        return json.loads(result.stdout) if success and result.stdout.lstrip().startswith(b"{") else None

    def call(self, node, op, **params):
        packet = {"op": op, **params}
        if op not in ("prepare", "confirm_batch", "accept_batch"):
            return node.client.call("delivery", packet)
        def attempt():
            response = node.client.command("request", method="delivery", params=packet, connection="main")
            error = response.get("error")
            if error == "another delivery operation is in progress":
                return None
            need(not error, "delivery_" + op + "_rejected")
            return {"result": response.get("result")}
        return self.wait("delivery-" + op + "-after-explicit-pre-mutation-busy", attempt)["result"]

    def rows(self, node, query, parameters=()):
        path = node.directory / "delivery/service.sqlite3"
        if not path.exists():
            return []
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as database:
            database.execute("PRAGMA query_only=ON")
            return database.execute(query, parameters).fetchall()

    def scan_receipts(self):
        for node in self.nodes:
            for statement_digest, filename in self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest"):
                key = (node.name, statement_digest)
                path = Path(filename)
                need(path.is_absolute() and path.resolve().is_relative_to(node.directory.resolve()), "receipt_must_belong_to_this_fresh_node")
                self.receipt_seen.setdefault(key, time.monotonic())
                if key in self.proofs or not path.exists():
                    continue
                receipt = read_regular(path, 16384, private=True)
                journal = extract_journal(receipt, statement_digest)
                fields = words(journal)
                relation = int.from_bytes(fields[1], "big")
                need(relation in (1, 2, 3), "unexpected_open_or_other_proof")
                need(fields[4].hex() == self.profile_id and fields[5] == bytes(self.profile["guest_image_id"])
                     and int.from_bytes(fields[2], "big") == 31337
                     and "0x" + fields[3][-20:].hex() == self.deployment["escrow"].lower(), "exact_public_statement_identity_required")
                order_id = fields[6].hex()
                if relation != 1:
                    need(order_id in self.orders and int.from_bytes(fields[25], "big") == 1
                         and int.from_bytes(fields[26], "big") == 0 and int.from_bytes(fields[16], "big") == 0,
                         "new_single_window_recoverable_order_required")
                    need([int.from_bytes(fields[index], "big") for index in (33, 34, 35, 36)] == [5, 0, 0, 5], "genuine_exact_fee5_terms_required")
                env = {"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": str(self.paths["r0vm"])}
                verified = self.command([self.paths["prover"], "verify", journal.hex()], data=receipt, timeout=120, environment=env)
                result = json.loads(verified.stdout)
                need(result.get("verified") is True and result.get("kind") == "groth16"
                     and result.get("imageId") == bytes(self.profile["guest_image_id"]).hex()
                     and result.get("journalHash") == statement_digest
                     and isinstance(result.get("seal"), str) and re.fullmatch(r"[0-9a-f]+", result["seal"]), "official_host_groth16_identity_required")
                negative = self.command([self.paths["prover"], "verify", wrong_journal(journal).hex()],
                    data=receipt, timeout=120, environment=env, success=False)
                need(b"receipt journal mismatch" in negative.stderr, "wrong_journal_must_fail_at_official_receipt_comparison")
                basename = "key-binding" if relation == 1 else f"order-{order_id}-relation-{relation}-window-0"
                need(not (self.public / (basename + ".receipt")).exists(), "fresh_unique_proof_export_required")
                durable(self.public / (basename + ".receipt"), receipt)
                metadata = {"mode": "prove", "kind": "groth16", "relation": relation,
                            "imageId": result["imageId"], "profileId": self.profile_id, "journal": journal.hex(),
                            "journalHash": statement_digest, "seal": result["seal"]}
                save_json(self.public / (basename + ".json"), metadata)
                entry = {"relation": relation, "public": basename + ".json", "receipt": basename + ".receipt"}
                if relation != 1:
                    entry.update({"orderId": "0x" + order_id, "windowIndex": 0})
                fact = {**entry, "node": node.name, "journalHash": statement_digest, "receiptSha256": digest(receipt),
                        "publicSha256": file_digest(self.public / (basename + ".json")),
                        "official_host_verified": True, "wrong_journal_receipt_comparison_rejected": True,
                        "first_reservation_observed_to_export_seconds": round(time.monotonic() - self.receipt_seen[key], 3)}
                self.proofs[key] = {"entry": entry, "journal": journal, "fact": fact}
                self.summary["genuine_receipts"].append(fact)
                self.progress()

    def proof_count(self, relation):
        return sum(item["entry"]["relation"] == relation for item in self.proofs.values())

    def public_chain(self, *, orders=False, capsules=False):
        tx_hashes = []
        for node in self.nodes:
            for _, tx_hash, phase in self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id"):
                need(phase not in ("reverted", "failed", "conflicted"), "actual_product_transaction_terminal_failure")
                if phase == "confirmed":
                    need(re.fullmatch(r"0x[0-9a-f]{64}", tx_hash), "actual_confirmed_transaction_hash_required")
                    tx_hashes.append(tx_hash)
        deployment = self.deployment
        packet = {"repository": str(self.repository), "rpc_url": deployment["rpc_url"], "escrow": deployment["escrow"],
                  "asset": deployment["asset"], "transactions": sorted(set(tx_hashes)),
                  "orders": ["0x" + value for value in self.orders] if orders else [],
                  "accounts": [node.wallet_address for node in self.nodes] + [deployment["escrow"]],
                  "budget_id": "0x" + self.budget if self.budget else None, "include_capsules": capsules,
                  "contracts": {name: deployment[name] for name in ("escrow", "asset", "adapter", "verifier")}}
        output = self.command(["node", self.repository / "contracts/delivery/inspect-protected.mjs"], data=json.dumps(packet).encode(), timeout=120)
        return json.loads(output.stdout)

    def get(self, node, order_id):
        value = self.call(node, "get", order_id=order_id)
        if value.get("error"):
            save_json(self.run / ("order-failure-" + order_id + ".json"), value)
            message = str(value["error"]).lower()
            self.summary["failure_order_id"] = order_id
            code = "actual_paid_order_executor_failed_private_log_retained"
            if re.search(r"proof|prover", message) and re.search(r"timeout|timed out", message):
                code = "actual_product_proof_timeout_with_original_7200_second_cap"
            raise CheckFailure(code)
        need(value["deadlines"] == self.orders[order_id]["deadlines"], "original_order_deadlines_must_not_change")
        return value

    def start(self):
        need(sys.platform.startswith("linux") and platform.machine() in ("x86_64", "amd64"), "linux_amd64_required")
        need(not any(name.startswith(("RISC0_DEV", "RISC0_SKIP")) for name in os.environ), "no_dev_or_skip_verifier_environment")
        git_env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
        source = self.command(["git", "rev-parse", "HEAD"], environment=git_env).stdout.decode().strip()
        dirty = self.command(["git", "status", "--porcelain", "--untracked-files=normal"], environment=git_env).stdout
        need(source == self.config["source_commit"] and not dirty, "final_clean_product_head_required")
        profile_path = self.repository / "crates/delivery/proof/profile.json"
        targets = {name: path for name, path in self.paths.items() if name != "repository"}
        targets.update({"crypto_wasm": self.paths["crypto_module"].with_name("vibeface_secure_bg.wasm"), "profile": profile_path})
        for name, path in targets.items():
            need(path.is_file() and not path.is_symlink() and file_digest(path) == self.config["sha256"][name], "final_artifact_hash_mismatch_" + name)
        self.profile = json.loads(read_regular(profile_path, 16384))
        self.profile_id = profile_id(self.profile)
        need(len(bytes(self.profile["guest_image_id"])) == 32 and any(self.profile["guest_image_id"])
             and [self.profile[name] for name in PROFILE_FIELDS] == [1, 2, 65536, 131072, 64000000, 17179869184],
             "exact_official_pinned_profile_required")
        need(digest((self.repository / "crates/delivery/abi/Statement.sol").read_bytes()) == bytes(self.profile["statement_schema_hash"]).hex(), "tracked_47_word_schema_identity_required")
        info_result = self.command([self.paths["prover"], "info"], timeout=30,
                                  environment={"PATH": "/usr/bin:/bin", "RISC0_SERVER_PATH": str(self.paths["r0vm"])})
        info = json.loads(info_result.stdout)
        need(info == {"imageId": bytes(self.profile["guest_image_id"]).hex(), "schemaHash": bytes(self.profile["statement_schema_hash"]).hex(),
                      "elfSha256": self.config["elf_sha256"], "statementBytes": 1504}, "official_guest_host_profile_identity_required")
        self.command(["python3", "scripts/build-inputs.py", "check", "crypto"], timeout=120)
        cgroup, self.initial_resources = resources()
        self.summary.update({"profile_id": self.profile_id, "prover_info": info, "initial_resources": self.initial_resources})
        self.prover_settings = {"program": str(self.paths["prover"]), "r0vm": str(self.paths["r0vm"]), "cgroup": str(cgroup), "profile": self.profile}
        self.progress("fresh-current-clock-deployment")
        deployment_file = self.run / "deployment.json"
        log = (self.run / "deployment.log").open("xb")
        self.deployment_process = subprocess.Popen(["node", str(self.repository / "contracts/delivery/deploy-local.mjs"), "--listen", "127.0.0.1:0", "--output", str(deployment_file)],
                                                  stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, env=self.environment)
        log.close()
        self.processes.capture()
        def ready():
            need(self.deployment_process.poll() is None, "fresh_deployment_exited_private_log_retained")
            try:
                value = json.loads(deployment_file.read_bytes())
                return value if value.get("rpc_url") else None
            except (FileNotFoundError, json.JSONDecodeError):
                return None
        self.deployment = self.wait("fresh-current-clock-deployment", ready)
        need(self.deployment["chain_id"] == 31337 and self.deployment["confirmations"] == 2
             and self.deployment["profile"] == self.profile and "experiment_fixture" not in self.deployment,
             "fresh_current_clock_official_deployment_required")
        loopback(self.deployment["rpc_url"])
        latest = self.rpc("eth_getBlockByNumber", ["latest", False])
        need(abs(int(latest["timestamp"], 16) - time.time()) < 60, "real_current_chain_clock_required")
        self.setup_nodes()

    def setup_nodes(self):
        self.progress("default-off-owner-configuration-and-fresh-wallets")
        workspace = self.run / "workspace"
        workspace.mkdir(mode=0o700)
        client = self.run / "client.mjs"
        durable(client, self.helpers["CLIENT_SOURCE"].replace("MODULE_URL", self.paths["crypto_module"].as_uri()).encode())
        self.relay = self.helpers["PkarrRelay"]()
        accounts = self.rpc("eth_accounts", [])
        need(len(accounts) == 4 and accounts[0].lower() == self.deployment["funder"].lower(), "fresh_testchain_funder_only")
        need(int(self.rpc("eth_chainId", []), 16) == 31337, "testchain_identity_required")
        self.funder = accounts[0]
        default_checks = []
        for name in ("S", "R", "P"):
            args = self.helpers["delivery_entry_args"]() if name == "P" else []
            node = self.helpers["Node"](self.run, name, self.paths["binary"], workspace, client, init_args=args)
            self.nodes.append(node)
            self.helpers["isolate_delivery_node"](node)
            node.configure_network(pkarr={"publish": True, "resolve": True, "urls": [self.relay.url], "publish_direct": True})
            default = json.loads((node.directory / "config.json").read_bytes())["delivery"]
            need(default["enabled"] is False and default["paid_enabled"] is False, "lab_paid_default_off_required")
            node.start()
            if name != "P":
                node.bootstrap()
                status = self.call(node, "status")
                need(status["enabled"] is False and status["paid_enabled"] is False and status["wallet"]["state"] == "absent", "default_disabled_api_and_absent_wallet_required")
            need(self.cli(node, ["status"])["state"] == "absent" and not (node.directory / "delivery-wallet.json").exists()
                 and not self.rows(node, "SELECT id FROM transactions"), "default_off_creates_no_wallet_or_payment")
            default_checks.append({"node": name, "lab_enabled": False, "paid_enabled": False, "wallet": "absent", "transactions": 0})
            settings = {"enabled": True, "charity_enabled": False, "paid_enabled": True,
                        "chain": {field: self.deployment[field] for field in ("rpc_url", "chain_id", "escrow", "asset", "profile", "confirmations")},
                        "prover": self.prover_settings, "provider_gas_cap": CAP, "publication_policy": POLICY,
                        "delivery_fee_per_window": "5", "reservation_fee_per_window": "0", "priority_fee": "0"}
            path = node.directory / "acceptance-delivery-settings.json"
            save_json(path, settings)
            self.cli(node, ["configure", str(path)])
            password = base64.urlsafe_b64encode(os.urandom(32)).decode()
            self.passwords[name] = password
            need(self.cli(node, ["create"], password=password)["state"] == "locked", "new_wallet_starts_locked")
            self.cli(node, ["unlock"], password="wrong-task-passphrase", success=False)
            need(self.cli(node, ["status"])["state"] == "locked", "wrong_passphrase_cannot_unlock")
            unlocked = self.cli(node, ["unlock"], password=password)
            need(unlocked["state"] == "unlocked", "owner_fd_wallet_unlock_required")
            node.wallet_address = unlocked["address"].lower()
            funding = self.rpc("eth_sendTransaction", [{"from": self.funder, "to": node.wallet_address, "value": hex(10 ** 19)}])
            self.wait("fresh-test-native-funding-" + name, lambda: self.confirmed(funding), timeout=60)
        source, receiver, provider = self.nodes
        need(source.client.network_id != receiver.client.network_id and len({node.device_id for node in self.nodes}) == 3
             and len({node.wallet_address for node in self.nodes}) == 3, "independent_s_r_p_network_identity_wallets_required")
        transfer = "0xa9059cbb" + source.wallet_address[2:].zfill(64) + format(10, "064x")
        token_tx = self.rpc("eth_sendTransaction", [{"from": self.funder, "to": self.deployment["asset"], "data": transfer}])
        self.wait("fresh-testtoken-funding", lambda: self.confirmed(token_tx), timeout=60)
        initial = self.public_chain()
        need(initial["balances"][source.wallet_address] == "10" and initial["balances"][receiver.wallet_address] == "0"
             and initial["balances"][provider.wallet_address] == "0" and initial["balances"][self.deployment["escrow"].lower()] == "0",
             "fresh_known_testtoken_balances_required")
        self.summary["token_balances_before"] = initial["balances"]
        self.summary.update({"default_off_checks": default_checks, "principals": {node.name: {"peer": node.device_id, "wallet": node.wallet_address,
                             "network_id": node.client.network_id if node.name != "P" else None} for node in self.nodes},
                             "deployment": {field: self.deployment[field] for field in ("rpc_url", "chain_id", "escrow", "asset", "adapter", "verifier", "runtime_code_hashes", "baseline_block")}})
        save_json(self.run / "private-runtime.json", {"passwords": self.passwords, "nodes": {node.name: str(node.directory) for node in self.nodes}})

    def register(self):
        receiver = self.nodes[1]
        intent = str(uuid.uuid4())
        arguments = ["register-key", "--intent-id", intent, "--gas-cap", CAP]
        output = self.run / "register-key-output.json"
        errors = self.run / "register-key-private.log"
        with output.open("xb") as stdout, errors.open("xb") as stderr:
            process = subprocess.Popen([str(self.paths["binary"]), "wallet", "--data-dir", str(receiver.directory), *arguments],
                stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True)
        def done():
            if process.poll() is None:
                return None
            need(process.returncode == 0, "genuine_register_key_failed_private_log_retained")
            return True
        self.wait("genuine-register-key-before-order-deadlines", done, timeout=7400)
        def confirmed():
            value = self.cli(receiver, arguments)
            need(value.get("phase") not in ("failed", "reverted", "conflicted", "superseded"), "genuine_key_registry_terminal_failure")
            return value if value.get("phase") == "confirmed" and value.get("registry_confirmed") is True else None
        result = self.wait("register-key-confirmed-before-any-prepare", confirmed)
        self.scan_receipts()
        need(self.proof_count(1) == 1 and len(self.proofs) == 1, "exactly_one_new_key_binding_receipt_required")
        self.summary["registration"] = {"intent_id": intent, "serial": result["serial"], "binding_hash": result["binding_hash"], "tx_hash": result["tx_hash"],
                                        "confirmed_before_any_order_prepare": True}

    def open_and_prepare(self):
        source, receiver, provider = self.nodes
        intent = str(uuid.uuid4())
        arguments = ["budget", "open", "--intent-id", intent, "--payee", provider.wallet_address, "--provider-peer", provider.device_id,
                     "--deposit", "10", "--gas-cap", CAP]
        def opened():
            value = self.cli(source, arguments)
            need(re.fullmatch(r"[0-9a-f]{64}", value["budget_id"]), "actual_budget_id_required")
            need(self.budget is None or value["budget_id"] == self.budget, "budget_open_same_uuid_identity")
            self.budget = value["budget_id"]
            need(value["transaction"]["phase"] not in ("reverted", "conflicted", "failed"), "budget_open_terminal_failure")
            return value if value["transaction"]["phase"] == "confirmed" else None
        self.wait("confirmed-prepaid-budget-before-orders", opened)
        original = os.urandom(4096)
        durable(self.run / "synthetic-original.bin", original)
        self.original = original
        for label in ("a", "b"):
            name = "native-genuine-" + label + ".bin"
            intent = str(uuid.uuid4())
            prepared = self.call(source, "prepare", intent_id=intent, receiver_peer=receiver.device_id, provider_peer=provider.device_id,
                mode="paid", acceptance="recoverable", name=name, content_base64=base64.b64encode(original).decode(),
                max_service_amount="5", max_gas_amount=CAP, budget_id=self.budget)
            order_id = prepared["order_id"]
            need(re.fullmatch(r"[0-9a-f]{64}", order_id) and order_id not in self.orders and prepared["intent_id"] == intent,
                 "two_distinct_fresh_paid_order_ids_required")
            need(prepared["mode"] == "paid" and prepared["acceptance"] == "recoverable" and len(prepared["windows"]) == 1
                 and prepared["payment"]["budget_id"] == self.budget and prepared["payment"]["total"] == "5"
                 and prepared["payment"]["delivery_fee"] == "5" and prepared["payment"]["reservation_fee"] == "0"
                 and prepared["payment"]["priority_fee"] == "0", "exact_paid_recoverable_fee5_shared_budget_required")
            self.orders[order_id] = {"order_id": order_id, "intent_id": intent, "label": label, "name": name, "bytes": len(original),
                "sha256": digest(original), "deadlines": prepared["deadlines"], "fees": {"delivery": "5", "reservation": "0", "bonus": "0"}}
        self.summary["budget_id"] = self.budget
        self.summary["orders"] = list(self.orders.values())
        self.progress("two-new-original-deadlines-fixed")
        self.call(source, "confirm_batch", intent_ids=[item["intent_id"] for item in self.orders.values()])

    def wait_allocations_and_stop_source(self):
        source, receiver, provider = self.nodes
        def uploaded():
            for order_id in self.orders:
                self.get(source, order_id)
                records = self.rows(provider, "SELECT idx,kind,committed,complete FROM objects WHERE order_id=? ORDER BY idx,kind", (order_id,))
                if len(records) != 2 or any(index != 0 or not complete or committed <= 0 for index, _, committed, complete in records):
                    return None
                need({kind for _, kind, _, _ in records} == {"ciphertext", "capsule"}, "provider_has_both_real_c_e_objects")
            received = {item["order_id"] for item in self.call(receiver, "list")["orders"]}
            return True if set(self.orders).issubset(received) and self.proof_count(2) == 2 else None
        self.wait("two-genuine-source-allocations-complete-upload-and-offers", uploaded, timeout=16000)
        snapshot = self.public_chain(orders=True)
        slots = {("0x" + value, 0) for value in self.orders}
        reserves = [item for item in snapshot["transactions"] if item["method"] == "reserveBatch"]
        need(any({(slot["orderId"], slot["windowIndex"]) for slot in item["slots"]} == slots and len(item["slots"]) == 2 for item in reserves),
             "one_actual_reserve_batch_must_cover_two_distinct_orders")
        self.compare_public_values(snapshot, relations=(2,))
        need(snapshot["budget"]["locked"] == "10" and snapshot["budget"]["earned"] == "0", "pre_service_irrevocable_exact_allocation_required")
        need(not any(item["method"] in ("holdBatch", "revealBatch", "withdrawBudget") for item in snapshot["transactions"]), "no_early_provider_earned_or_payment")
        need(all(phase == "confirmed" for _, _, phase in self.rows(source, "SELECT id,hash,phase FROM transactions")), "source_has_no_unknown_payment_before_offline")
        source_nonce = int(self.rpc("eth_getTransactionCount", [source.wallet_address, "latest"]), 16)
        self.cli(source, ["lock"])
        need(self.cli(source, ["status"])["state"] == "locked", "payer_wallet_locked_before_offline")
        source.client.close()
        source.stop()
        need(source.process is None, "payer_process_stopped_after_real_upload_and_allocation")
        self.summary["source_offline"] = {"wallet_nonce": source_nonce, "after_two_slot_reserve": True,
            "after_both_c_e_objects_complete": True, "after_both_receiver_offers": True, "before_receiver_accept": True,
            "wall_clock_unix_seconds": int(time.time()), "chain_anchor": snapshot["anchor"]}
        self.public_json("before-source-offline.json", snapshot)
        self.checkpoint("source-offline-before-receiver-accept")

    def compare_public_values(self, snapshot, *, relations=(2, 3)):
        for window in snapshot["windows"]:
            for relation in relations:
                proof = [item for item in self.proofs.values() if item["entry"].get("orderId") == window["orderId"] and item["entry"]["relation"] == relation]
                need(len(proof) == 1 and proof[0]["journal"].hex() == window["public_values"][str(relation)], "exported_receipt_must_equal_product_public_values")

    def holds_and_settle(self):
        source, receiver, provider = self.nodes
        self.call(receiver, "accept_batch", order_ids=list(self.orders))
        def held():
            for order_id in self.orders:
                self.get(receiver, order_id)
            status = self.cli(provider, ["budget", "status", "--budget-id", self.budget])
            expected = {(self.budget, order_id, 0) for order_id in self.orders}
            ready = status["ready_windows"]
            actual = {(item["budget_id"], item["order_id"], item["index"]) for item in ready}
            return True if self.proof_count(3) == 2 and len(ready) == 2 and actual == expected else None
        self.wait("two-genuine-holds-private-before-explicit-flush", held, timeout=16000)
        before = self.public_chain(orders=True)
        need(not any(item["method"] in ("holdBatch", "revealBatch", "withdrawBudget") for item in before["transactions"]), "provider_batch_must_wait_for_explicit_flush")
        self.cli(provider, ["budget", "flush", "--budget-id", self.budget])
        def settled():
            values = [self.get(receiver, order_id) for order_id in self.orders]
            if not all(value["evidence"] == "recoverable" and value["payment"]["earned"] == "5" for value in values):
                return None
            state = self.cli(provider, ["budget", "status", "--budget-id", self.budget])
            budgets = state["budgets"]
            need(len(budgets) == 1 and budgets[0]["terms"]["budget_id"] == self.budget, "single_actual_shared_budget_status")
            budget = budgets[0]
            return budget if budget["earned"] == "10" and budget["withdrawn"] == "10" else None
        budget = self.wait("provider-independent-reveal-earned-and-cumulative-withdraw10", settled, timeout=300)
        need(source.process is None, "payer_stays_offline_through_payment_and_recovery")
        need(int(self.rpc("eth_getTransactionCount", [source.wallet_address, "latest"]), 16) == self.summary["source_offline"]["wallet_nonce"],
             "payer_nonce_must_not_increase_after_offline")
        reads = []
        for order_id, original in self.orders.items():
            value = self.call(receiver, "read", order_id=order_id)
            recovered = base64.b64decode(value["content_base64"], validate=True)
            need(recovered == self.original and value["name"] == original["name"] and value["bytes"] == original["bytes"]
                 and value["sha256"] == original["sha256"], "real_receiver_file_name_length_and_hash_must_match")
            order = self.get(receiver, order_id)
            need(order["payment"]["earned"] == "5" and order["payment"]["withdrawn"] == "0"
                 and not any(item["decryption_proven"] for item in order["windows"]), "per_order_earned_and_budget_withdraw_are_distinct")
            reads.append({"order_id": order_id, "name": value["name"], "bytes": value["bytes"], "sha256": value["sha256"], "payer_offline": True})
        snapshot = self.public_chain(orders=True, capsules=True)
        self.compare_public_values(snapshot)
        need(int(snapshot["balances"][provider.wallet_address]) - int(self.summary["token_balances_before"][provider.wallet_address]) == 10
             and snapshot["balances"][self.deployment["escrow"].lower()] == "0"
             and snapshot["budget"]["earned"] == "10" and snapshot["budget"]["withdrawn"] == "10"
             and snapshot["budget"]["locked"] == "0" and snapshot["budget"]["refunded"] == "0", "real_testtoken_conservation_and_payee_increment10")
        self.summary["recovered_files"] = reads
        self.summary["budget_final"] = {name: budget[name] for name in ("locked", "earned", "withdrawn", "refunded", "free", "state")}
        self.summary["payer_post_service_nonce_delta"] = 0
        self.export_settlement(snapshot)

    def export_settlement(self, snapshot):
        source, receiver, provider = self.nodes
        callers = {"registerKey": receiver.wallet_address, "approve": source.wallet_address, "openBudget": source.wallet_address,
                   "reserveBatch": source.wallet_address, "holdBatch": provider.wallet_address, "revealBatch": provider.wallet_address, "withdrawBudget": provider.wallet_address}
        transactions = {}
        for method, caller in callers.items():
            selected = [item for item in snapshot["transactions"] if item["method"] == method]
            need(selected and all(item["from"] == caller for item in selected), "actual_budget_methods_and_fixed_callers_required")
            if method in ("registerKey", "approve", "openBudget"):
                need(len(selected) == 1, "single_registration_approval_and_budget_open_required")
                transactions[method] = selected[0]["hash"]
            else:
                transactions[method] = [item["hash"] for item in selected]
        slots = {("0x" + value, 0) for value in self.orders}
        for method in ("reserveBatch", "holdBatch", "revealBatch"):
            selected = [item for item in snapshot["transactions"] if item["method"] == method]
            need(any(len(item["slots"]) == 2 and {(slot["orderId"], slot["windowIndex"]) for slot in item["slots"]} == slots for item in selected), "actual_two_distinct_slot_" + method + "_required")
        need(len(transactions["withdrawBudget"]) == 1 and len(self.proofs) == 5 and self.proof_count(1) == 1
             and self.proof_count(2) == 2 and self.proof_count(3) == 2, "five_new_receipts_one_cumulative_withdraw_required")
        bundle = {"schemaVersion": 2, "escrow": self.deployment["escrow"], "asset": self.deployment["asset"], "budgetId": "0x" + self.budget,
                  "orders": [{"orderId": "0x" + value, "windowCount": 1, "amount": "5"} for value in self.orders],
                  "proofs": [item["entry"] for item in self.proofs.values()],
                  "capsules": [{name: item[name] for name in ("orderId", "windowIndex", "capsule")} for item in snapshot["windows"]], "transactions": transactions}
        self.public_json("settlement.json", bundle)
        self.public_json("confirmed-chain.json", snapshot)
        observed = self.command(["node", self.repository / "contracts/delivery/observe-settlement.mjs", "--settlement", self.public / "settlement.json",
                                 "--rpc-url", self.deployment["rpc_url"], "--case", "protected"], timeout=180)
        value = json.loads(observed.stdout)
        need(value.get("status") == "PASS", "official_protected_settlement_observer_required")
        self.public_json("settlement-observation.json", value)
        self.summary["official_settlement_observer"] = "PASS"

    def restart_and_disabled_boundaries(self):
        self.progress("real-s-r-p-restart-locked-disabled-history")
        previous = {node.name: self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id") for node in self.nodes}
        previous_receipts = {node.name: self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in self.nodes}
        for node in self.nodes:
            if node.process is not None:
                self.cli(node, ["lock"])
                node.client.close()
                node.stop()
            node.start()
            need(self.cli(node, ["status"])["state"] == "locked", "restarted_real_wallet_must_be_locked")
            if node.name != "P":
                node.client.connect()
                node.client.authenticate()
            settings_path = node.directory / "acceptance-delivery-settings.json"
            settings = json.loads(settings_path.read_bytes())
            settings.update({"enabled": False, "paid_enabled": False})
            save_json(settings_path, settings)
            self.cli(node, ["configure", str(settings_path)])
            self.cli(node, ["budget", "withdraw", "--budget-id", self.budget, "--intent-id", str(uuid.uuid4())], success=False)
            status = self.cli(node, ["budget", "status", "--budget-id", self.budget])
            need(status["execution_enabled"] is False, "disabled_locked_budget_status_is_read_only")
        for order_id in self.orders:
            value = self.call(self.nodes[1], "read", order_id=order_id)
            need(base64.b64decode(value["content_base64"], validate=True) == self.original, "restart_disabled_history_still_recovers_original")
        need(previous == {node.name: self.rows(node, "SELECT id,hash,phase FROM transactions ORDER BY id") for node in self.nodes}
             and previous_receipts == {node.name: self.rows(node, "SELECT digest,path FROM proof_jobs ORDER BY digest") for node in self.nodes},
             "restart_and_disabled_operations_must_not_repeat_proof_or_business_write")
        self.summary["real_process_restart"] = {"nodes": ["S", "R", "P"], "wallets_locked": True, "lab_disabled": True,
            "original_receipts_and_transactions_unchanged": True, "receiver_history_read": True}

    def execute(self):
        try:
            self.start()
            self.register()
            self.open_and_prepare()
            self.wait_allocations_and_stop_source()
            self.holds_and_settle()
            self.restart_and_disabled_boundaries()
            git_env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
            need(self.command(["git", "rev-parse", "HEAD"], environment=git_env).stdout.decode().strip() == self.config["source_commit"]
                 and not self.command(["git", "status", "--porcelain", "--untracked-files=normal"], environment=git_env).stdout,
                 "product_head_must_remain_clean_and_unchanged_for_entire_run")
            self.checkpoint("final")
            self.summary["status"] = "PASS"
        except BaseException as error:
            self.summary["status"] = "INTERRUPTED" if isinstance(error, (KeyboardInterrupt, SystemExit)) else "FAIL"
            self.summary["failure_code"] = str(error) if isinstance(error, CheckFailure) else type(error).__name__
            durable(self.run / "failure-private.log", traceback.format_exc().encode())
            try:
                self.scan_receipts()
            except BaseException:
                durable(self.run / "partial-export-failure-private.log", traceback.format_exc().encode())
        finally:
            survivors = self.processes.stop_all()
            for node in self.nodes:
                try:
                    node.client.close()
                    node.stop()
                except BaseException:
                    pass
            if getattr(self, "deployment_process", None) is not None:
                try:
                    self.deployment_process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.summary["status"] = "FAIL"
                    self.summary["failure_code"] = "deployment_child_cleanup_incomplete"
            if self.relay is not None:
                self.relay.close()
            if survivors:
                self.summary["status"] = "FAIL"
                self.summary["failure_code"] = "owned_child_cleanup_incomplete"
                self.summary["owned_child_survivors"] = survivors
            if self.initial_resources:
                try:
                    _, final = resources()
                    self.summary["final_resources"] = final
                    self.summary["oom_delta"] = oom_delta(self.initial_resources, final)
                    if any(self.summary["oom_delta"].values()):
                        self.summary["status"] = "FAIL"
                        self.summary["failure_code"] = "actual_cgroup_oom_during_experiment"
                except BaseException:
                    self.summary["status"] = "FAIL"
                    self.summary["failure_code"] = "final_resource_read_failed"
            self.progress()
            self.public_json("native-result.json", self.summary)
            self.progress()
        return 0 if self.summary["status"] == "PASS" else 1


def run_experiment(config, helpers, run_directory):
    os.umask(0o077)
    previous = signal.getsignal(signal.SIGTERM)
    def interrupt(_signum, _frame):
        raise KeyboardInterrupt()
    signal.signal(signal.SIGTERM, interrupt)
    try:
        return Experiment(config, helpers, run_directory).execute()
    finally:
        signal.signal(signal.SIGTERM, previous)



def run_from_options(options, helpers):
    need(options.delivery_config is not None, "owner_only_final_delivery_config_required")
    path = options.delivery_config
    need("trash" not in str(path).lower(), "forbidden_configuration_path")
    config = validate_config(json.loads(read_regular(path.resolve(strict=True), 16384, private=True)))
    for name in ("binary", "core_binary", "crypto_module"):
        need(Path(config["paths"][name]).resolve(strict=True) == getattr(options, name).resolve(strict=True), "cli_option_and_final_config_artifact_mismatch")
    run_directory = options.delivery_run_dir or options.artifacts.resolve() / ("delivery-protected-" + uuid.uuid4().hex)
    return run_experiment(config, helpers, run_directory)


def self_test():
    from unittest.mock import Mock, patch
    from types import SimpleNamespace
    import socket
    def forbidden(*_args, **_kwargs):
        raise AssertionError("no-process self-test attempted an external process or network")
    with patch.object(subprocess, "Popen", forbidden), patch.object(subprocess, "run", forbidden), patch.object(socket, "socket", forbidden):
        sample = {"schema_version": 1, "source_commit": "1" * 40,
            "paths": {"repository": str(REPOSITORY), "binary": "/artifact/vibeface", "core_binary": "/artifact/vibeface-core", "crypto_module": "/artifact/vibeface_secure.js", "prover": "/artifact/vibeface-delivery-prover", "r0vm": "/artifact/r0vm"},
            "sha256": {name: "a" * 64 for name in ("binary", "core_binary", "crypto_module", "crypto_wasm", "prover", "r0vm", "profile")},
            "elf_sha256": "b" * 64, "caps": {"deposit": "10", "max_service_amount": "5", "source_gas_cap_wei": CAP, "provider_gas_cap_wei": CAP, "register_key_gas_cap_wei": CAP, "native_funding_wei_each": str(10 ** 19)}, "publication_policy": POLICY}
        validate_config(sample)
        for change in (lambda item: item.update(source_commit="0" * 40), lambda item: item.update(private_key="forbidden"),
                       lambda item: item["paths"].update(binary="/unrelated/trash/vibeface"), lambda item: item["sha256"].update(prover="0" * 64),
                       lambda item: item["publication_policy"].update(clock_skew_seconds=0), lambda item: item["caps"].update(deposit="11")):
            invalid = copy.deepcopy(sample); change(invalid)
            try: validate_config(invalid)
            except CheckFailure: pass
            else: raise AssertionError("invalid protected config accepted")
        journal = bytes(range(256)) * 5 + bytes(range(224))
        need(extract_journal(b"prefix" + journal + b"suffix", digest(journal)) == journal, "unique_public_journal")
        for invalid in (b"missing", journal + journal, b"x" * 16385):
            try: extract_journal(invalid, digest(journal))
            except CheckFailure: pass
            else: raise AssertionError("ambiguous public journal accepted")
        for relation in (1, 2, 3):
            fields = [bytes([7]) * 32 for _ in range(47)]; fields[1] = relation.to_bytes(32, "big")
            need(len(wrong_journal(b"".join(fields))) == 1504, "wrong_journal_shape")
        for url in ("https://127.0.0.1:8545", "http://example.com:8545", "http://user@127.0.0.1:8545", "http://127.0.0.1:8545/path"):
            try: loopback(url)
            except (CheckFailure, ValueError): pass
            else: raise AssertionError("non-loopback origin accepted")
        need(oom_delta({"memory.events": "oom 0\noom_kill 0\n"}, {"memory.events": "oom 1\noom_kill 0\n"})["oom"] == 1, "actual_oom_delta")
        experiment = object.__new__(Experiment)
        experiment.progress = Mock(); experiment.scan_receipts = Mock(); experiment.checkpoint = Mock()
        experiment.processes = SimpleNamespace(capture=Mock())
        busy = {"error": "another delivery operation is in progress"}
        requests = {
            "prepare": {"intent_id": "00000000-0000-4000-8000-000000000001", "budget_id": "b" * 64},
            "confirm_batch": {"intent_ids": ["00000000-0000-4000-8000-000000000001", "00000000-0000-4000-8000-000000000002"]},
            "accept_batch": {"order_ids": ["a" * 64, "b" * 64]},
        }
        for op, params in requests.items():
            client = Mock(command=Mock(side_effect=[busy, {"result": {"accepted": True}}]))
            with patch("time.monotonic", return_value=0), patch("time.sleep"):
                need(experiment.call(SimpleNamespace(client=client), op, **params) == {"accepted": True}, "explicit_busy_then_success")
            need(client.command.call_count == 2 and client.command.call_args_list[0] == client.command.call_args_list[1], "busy_retry_keeps_exact_request")
            need(client.command.call_args.kwargs["params"] == {"op": op, **params}, "busy_retry_keeps_original_ids")
            client.call.assert_not_called()
            for error in (TimeoutError("network outcome unknown"), RuntimeError("another delivery operation is in progress"),
                          {"error": "outcome unknown"}, {"error": "another delivery operation is in progress; uncertain"}, {"error": "other refusal"}):
                client = Mock(command=Mock(side_effect=[error] if isinstance(error, Exception) else None, return_value=error))
                with patch("time.monotonic", return_value=0), patch("time.sleep") as pause:
                    try: experiment.call(SimpleNamespace(client=client), op, **params)
                    except (CheckFailure, TimeoutError, RuntimeError): pass
                    else: raise AssertionError("a non-busy failure was retried or accepted")
                need(client.command.call_count == 1, "non_busy_failure_is_not_retried"); pause.assert_not_called()
            client = Mock(command=Mock(return_value=busy))
            with patch("time.monotonic", side_effect=[0, 0, 0, 0, 121]), patch("time.sleep"):
                try: experiment.call(SimpleNamespace(client=client), op, **params)
                except CheckFailure as error: need("stage_observer_timeout_" in str(error), "busy_keeps_existing_stage_bound")
                else: raise AssertionError("busy exceeded the original stage bound")
            need(client.command.call_count == 1, "expired_busy_stage_sends_no_new_request")
        client = Mock(call=Mock(side_effect=AssertionError("another delivery operation is in progress")))
        try: experiment.call(SimpleNamespace(client=client), "get", order_id="a" * 64)
        except AssertionError: pass
        else: raise AssertionError("read operations must retain their original Client.call behavior")
        client.command.assert_not_called()
    print(json.dumps({"status": "SELF_TEST_PASS", "external_processes": 0, "network_sockets": 0, "genuine_receipts_generated": 0}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true", required=True)
    parser.parse_args()
    self_test()
