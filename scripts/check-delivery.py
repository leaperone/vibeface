#!/usr/bin/env python3
"""Rebuild the delivery guest, verify its public receipts, and exercise the EVM adapter."""

import argparse
import contextlib
import errno
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import uuid
import zipfile

from verification.resource import oom_delta, resources as cgroup_resources


ROOT = Path(__file__).resolve().parent.parent
ARTIFACTS = ROOT / ".artifacts/vdp"
FIXTURES = ROOT / "crates/delivery/proof/fixtures"
PROFILE = ROOT / "crates/delivery/proof/profile.json"
SHARED_TARGETS = ("proof-target", "domain-target")
SANDBOX_FILES = ("crates/delivery/proof/Sandbox.Dockerfile", "crates/delivery/proof/confined-gnark.sh")
STATEMENT_BYTES = 47 * 32
PUBLIC_FIELDS = {"mode", "kind", "imageId", "profileId", "relation", "journal", "journalHash", "seal"}
PROFILE_CAPS = {"crypto_suite": 1, "max_windows": 2, "max_window_bytes": 65_536, "max_total_bytes": 131_072,
                "max_guest_cycles": 64_000_000, "max_prover_memory_bytes": 16 * 1024 * 1024 * 1024}


def hex_bytes(value, length=None):
    if not isinstance(value, str) or any(character not in "0123456789abcdef" for character in value):
        raise ValueError("Public proof values must use lowercase hexadecimal without a prefix")
    if not value or len(value) % 2 or length is not None and len(value) != length * 2:
        raise ValueError("Public proof hexadecimal has the wrong length")
    return bytes.fromhex(value)

ITEMS = ("prepare", "relation-1", "relation-2", "relation-3", "relation-4", "accept")
WORKFLOW_PATH = ".github/workflows/delivery-small.yml"
DEADLINE_ENV = "VIBEFACE_DELIVERY_DEADLINE_UNIX_MS"
ITEM_CEILING_SECONDS = 5 * 60 * 60 + 30 * 60
PROOF_NAMES = {"manifest.json", *[f"relation-{relation}.{suffix}" for relation in range(1, 5) for suffix in ("json", "receipt")]}
CANONICAL = Path("/artifacts")
RESOURCE_SCOPE = "entire bounded sandbox including build, EVM, executor, prover and compression"


def remaining():
    deadline = os.environ.get(DEADLINE_ENV)
    if deadline is None:
        return None
    if not re.fullmatch(r"[0-9]{13}", deadline):
        raise ValueError("Invalid absolute work-item deadline")
    seconds = int(deadline) / 1000 - time.time()
    if seconds <= 0:
        raise TimeoutError("Delivery work-item deadline reached")
    return seconds


def checked_public(proof, relation, image_id):
    if set(proof) != PUBLIC_FIELDS or proof["mode"] != "prove" or proof["kind"] != "groth16" or type(proof["relation"]) is not int or proof["relation"] != relation:
        raise ValueError("Fixture must be the public output of a genuine Groth16 proof")
    if proof["imageId"] != image_id:
        raise ValueError("Fixture metadata binds another guest image")
    image = hex_bytes(proof["imageId"], 32)
    profile = hex_bytes(proof["profileId"], 32)
    journal = hex_bytes(proof["journal"], STATEMENT_BYTES)
    digest = hex_bytes(proof["journalHash"], 32)
    hex_bytes(proof["seal"], 4 + 8 * 32)
    if hashlib.sha256(journal).digest() != digest:
        raise ValueError("Fixture public journal digest mismatch")
    if int.from_bytes(journal[32:64], "big") != relation or journal[4 * 32:5 * 32] != profile or journal[5 * 32:6 * 32] != image:
        raise ValueError("Fixture relation, profile or image differs from its public journal")


def checked_metadata(manifest, proofs, compiled=None):
    if set(manifest) != {"schemaVersion", "imageId", "schemaHash", "elfSha256", "proofs"} or type(manifest["schemaVersion"]) is not int or manifest["schemaVersion"] != 1:
        raise ValueError("Unsupported delivery fixture manifest")
    for field in ("imageId", "schemaHash", "elfSha256"):
        hex_bytes(manifest[field], 32)
    if len(manifest["proofs"]) != 4 or len(proofs) != 4:
        raise ValueError("Delivery fixtures must cover all four proof relations")
    if compiled is not None:
        for field in ("imageId", "schemaHash", "elfSha256"):
            if manifest[field] != compiled[field]:
                raise ValueError(f"Delivery fixture {field} differs from the rebuilt guest")
        if compiled["statementBytes"] != STATEMENT_BYTES:
            raise ValueError("Rebuilt guest uses a different public statement schema")
    for relation, (entry, proof) in enumerate(zip(manifest["proofs"], proofs), 1):
        if entry != {"receipt": f"relation-{relation}.receipt", "public": f"relation-{relation}.json"}:
            raise ValueError("Unexpected delivery fixture file or order")
        checked_public(proof, relation, manifest["imageId"])


def read_fixture(name, limit, directory=None):
    file = (directory or FIXTURES) / name
    if Path(name).name != name or any(parent.is_symlink() for parent in (file, *file.parents)) or not file.is_file() or file.stat().st_nlink != 1 or file.stat().st_size > limit:
        raise ValueError(f"Missing, oversized or nonregular public fixture: {name}")
    return file.read_bytes()


def checked_profile(profile, compiled):
    if set(profile) != {"guest_image_id", "statement_schema_hash", *PROFILE_CAPS}:
        raise ValueError("Product delivery profile has unexpected fields")
    for field, expected in PROFILE_CAPS.items():
        if type(profile[field]) is not int or profile[field] != expected:
            raise ValueError(f"Product delivery profile has an unsupported {field}")
    for field, metadata in (("guest_image_id", "imageId"), ("statement_schema_hash", "schemaHash")):
        word = profile[field]
        if not isinstance(word, list) or len(word) != 32 or any(type(byte) is not int or not 0 <= byte <= 255 for byte in word):
            raise ValueError("Product delivery profile requires canonical 32-byte words")
        if bytes(word) != hex_bytes(compiled[metadata], 32):
            raise ValueError(f"Product delivery profile {field} differs from the rebuilt guest")


def load_profile(compiled):
    if PROFILE.is_symlink() or not PROFILE.is_file() or PROFILE.stat().st_size > 16 * 1024:
        raise ValueError("Missing, oversized or nonregular product delivery profile")
    profile = json.loads(PROFILE.read_bytes())
    checked_profile(profile, compiled)
    return profile


def checked_profile_vectors(vectors, profile, compiled, proofs):
    checked_profile(profile, compiled)
    if vectors["profile"] != profile or vectors["schemaHash"] != compiled["schemaHash"]:
        raise ValueError("Rust public vectors differ from the product delivery profile")
    expected = hex_bytes(vectors["profileId"], 32)
    if hashlib.sha256(hex_bytes(vectors["profileAbi"], 9 * 32)).digest() != expected:
        raise ValueError("Rust profile ID differs from its canonical ABI")
    for proof in proofs:
        if hex_bytes(proof["profileId"], 32) != expected:
            raise ValueError("Public receipt uses a profile outside the product trust root")


def load_fixtures(directory=None):
    manifest = json.loads(read_fixture("manifest.json", 16 * 1024, directory))
    proofs = [json.loads(read_fixture(f"relation-{relation}.json", 32 * 1024, directory)) for relation in range(1, 5)]
    checked_metadata(manifest, proofs)
    schema = (ROOT / "crates/delivery/abi/Statement.sol").read_bytes()
    if manifest["schemaHash"] != hashlib.sha256(schema).hexdigest():
        raise ValueError("Delivery fixtures use an outdated canonical ABI schema")
    load_profile(manifest)
    for relation in range(1, 5):
        if not read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024, directory):
            raise ValueError("An empty public receipt is not a proof")
    return manifest, proofs


def optional_fixtures():
    if not FIXTURES.exists() and not FIXTURES.is_symlink():
        print("skipped: no public proof fixture; receipt verification needs fixtures generated on an x86_64 machine with enough memory", flush=True)
        return None
    return load_fixtures()


def run(command, **kwargs):
    print("RUN delivery " + " ".join(str(part) for part in command[:2]), flush=True)
    budget = remaining()
    if budget is not None:
        cleanup_grace = 30 if command[:2] == ["docker", "run"] else 15 if command[:2] == ["node", "contracts/delivery/experiment-delivery.mjs"] else 0
        kwargs["timeout"] = min(kwargs.get("timeout", budget + cleanup_grace), budget + cleanup_grace)
    return subprocess.run(command, cwd=ROOT, check=True, **kwargs)


def checked_verification(result, compiled, proof):
    expected = {"verified": True, "kind": "groth16", "seal": proof["seal"], "imageId": compiled["imageId"], "journalHash": proof["journalHash"]}
    if result != expected or result["verified"] is not True:
        raise ValueError("Receipt verification result or exported seal differs from its public fixture")


def source_commit():
    if subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"], cwd=ROOT).strip():
        raise ValueError("Small experiments require a clean committed source checkout")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    hex_bytes(commit, 20)
    return commit


def shared_cache():
    common = Path(subprocess.check_output(["git", "rev-parse", "--path-format=absolute", "--git-common-dir"], cwd=ROOT, text=True).strip())
    if common.name != ".git" or not common.is_dir():
        return ARTIFACTS
    return common.parent / ".artifacts/vdp-shared"


def clone_tree(source, destination):
    flags = ["-cpR"] if sys.platform == "darwin" else ["-a", "--reflink=auto"]
    subprocess.check_call(["cp", *flags, str(source), str(destination)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def share_failed(error):
    return isinstance(error, (FileNotFoundError, FileExistsError)) or (type(error) is OSError and error.errno in (errno.ENOTEMPTY, errno.EEXIST, errno.EXDEV))


@contextlib.contextmanager
def deferred_sigterm():
    # os.walk and rmtree(ignore_errors=True) swallow the handler's InterruptedError; hold SIGTERM until they finish.
    previous = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM})
    try:
        yield
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous)


def seed_target(target, shared):
    if target.exists():
        return
    staging = target.with_name(f"{target.name}.seed-{uuid.uuid4().hex}")
    try:
        before = os.stat(shared)
        clone_tree(shared, staging)
        after = os.stat(shared)
        if (before.st_ino, before.st_dev, before.st_ctime_ns) != (after.st_ino, after.st_dev, after.st_ctime_ns):
            return
        with deferred_sigterm():
            # Cargo judges path crates and the guest by mtime, so another branch's outputs must look older than every source.
            for directory, directories, files in os.walk(staging, topdown=False):
                for name in directories + files:
                    os.utime(os.path.join(directory, name), (0, 0), follow_symlinks=False)
            os.utime(staging, (0, 0), follow_symlinks=False)
            os.rename(staging, target)
    except OSError as error:
        if not share_failed(error):
            raise
    except subprocess.CalledProcessError:
        pass
    finally:
        with deferred_sigterm():
            shutil.rmtree(staging, ignore_errors=True)


def publish_target(target, shared):
    if not target.is_dir():
        return
    token = uuid.uuid4().hex
    staging, old = shared.with_name(f"{shared.name}.tmp-{token}"), shared.with_name(f"{shared.name}.old-{token}")
    try:
        clone_tree(target, staging)
        try:
            os.rename(shared, old)
        except FileNotFoundError:
            pass
        os.rename(staging, shared)
    except OSError as error:
        if not share_failed(error):
            raise
    except subprocess.CalledProcessError:
        pass
    finally:
        with deferred_sigterm():
            shutil.rmtree(staging, ignore_errors=True)
            shutil.rmtree(old, ignore_errors=True)


def sandbox_commands(context, small=None, resume_dir=None, work_id=None, producer=None, cache=None):
    image = "vibeface-vdp-proof:check-" + hashlib.sha256(b"".join((ROOT / file).read_bytes() for file in SANDBOX_FILES)).hexdigest()[:12]
    build = ["docker", "build", "--platform", "linux/amd64", "--file", str(context / "crates/delivery/proof/Sandbox.Dockerfile"), "--tag", image, str(context)]
    container = ["docker", "run", "--rm", "--platform", "linux/amd64", "--memory", "11g", "--memory-swap", "11g", "--pids-limit", "64", "--env", "HOME=/home/runner", "--env", "PYTHONDONTWRITEBYTECODE=1"]
    cache = cache or ARTIFACTS
    mounts = [(ROOT, "/work"), (cache / "cargo", "/cargo"), (cache / "cargo", "/home/runner/.cargo"), (cache / "risc0", "/risc0"), (ARTIFACTS / "canonical", "/artifacts")]
    if cache != ARTIFACTS:
        mounts.append((cache / "risc0", "/work/.artifacts/vdp/risc0"))
    for source, destination in mounts:
        container.extend(["--mount", f"type=bind,source={source},target={destination}"])
    command = "python3 scripts/install-delivery-toolchain.py && python3 scripts/check-delivery.py"
    if small is not None:
        if small not in ITEMS:
            raise ValueError("Unsupported delivery work item")
        commit = source_commit()
        deadline = os.environ.get(DEADLINE_ENV)
        remaining()
        if deadline is None:
            raise ValueError("The host must set one absolute deadline before building")
        container.extend(["--env", "TOKIO_WORKER_THREADS=2", "--env", "VIBEFACE_DELIVERY_SOURCE_COMMIT=" + commit,
            "--env", DEADLINE_ENV + "=" + deadline, "--env", "VIBEFACE_DELIVERY_WORK_ID=" + work_id,
            "--env", "VIBEFACE_DELIVERY_PRODUCER=" + json.dumps(producer, separators=(",", ":"))])
        command += " --prove-small " + small
        if resume_dir is not None:
            relative = resume_dir.relative_to(ARTIFACTS / "canonical")
            if not re.fullmatch(r"incoming/[0-9a-f]{32}", relative.as_posix()):
                raise ValueError("Resume inputs must stay in the existing canonical mount")
            command += " --resume-dir /artifacts/" + relative.as_posix()
    container.extend([image, "bash", "-c", command])
    return build, container


def sandbox(small=None, resume_run=None):
    optional_fixtures()
    previous_deadline = os.environ.get(DEADLINE_ENV)
    work_id, producer, resume_dir = None, None, None
    public = None
    if small is not None:
        if small not in ITEMS:
            raise ValueError("Unsupported delivery work item")
        commit = source_commit()
        os.environ[DEADLINE_ENV] = str(int(time.time() * 1000) + ITEM_CEILING_SECONDS * 1000)
        work_id = uuid.uuid4().hex
        public = ARTIFACTS / "canonical/small" / work_id / "public"
    failure = None
    try:
        cache = shared_cache()
        for directory in ("cargo", "risc0"):
            (cache / directory).mkdir(parents=True, exist_ok=True)
        for directory in ("risc0", "canonical"):
            (ARTIFACTS / directory).mkdir(parents=True, exist_ok=True)
        if small is not None:
            (ARTIFACTS / "canonical/small").mkdir(mode=0o755, exist_ok=True)
            producer = current_producer(commit)
            if producer is None and (small != "prepare" or resume_run is not None):
                public = None
                raise ValueError("Proof and resume work items require authoritative Actions provenance")
            if producer is not None:
                resume_dir = download_inputs(producer, resume_run)
        with tempfile.TemporaryDirectory(prefix="sandbox-build-", dir=ARTIFACTS) as directory:
            context = Path(directory)
            for file in SANDBOX_FILES:
                source = ROOT / file
                if source.is_symlink() or not source.is_file():
                    raise ValueError(f"Sandbox build requires a regular source file: {file}")
                destination = context / file
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(source, destination)
            build, container = sandbox_commands(context, small, resume_dir, work_id, producer, cache)
            run(build)
        task = "".join(character if character.isascii() and (character.isalnum() or character in "_.-") else "-" for character in ROOT.name)[:64]
        name = "vdp-" + task + "-" + uuid.uuid4().hex
        container[2:2] = ["--name", name, "--label", "vibeface.task-root=" + str(ROOT)]
        print("Delivery sandbox owned by this task: " + name, flush=True)
        share = small is None and os.environ.get("GITHUB_ACTIONS") != "true" and cache != ARTIFACTS
        previous_term = signal.getsignal(signal.SIGTERM)

        def interrupted(*_):
            raise InterruptedError("Delivery sandbox interrupted")

        try:
            signal.signal(signal.SIGTERM, interrupted)
            for target in SHARED_TARGETS if share else ():
                seed_target(ARTIFACTS / "canonical" / target, cache / target)
            run(container)
            if small is not None and source_commit() != commit:
                raise ValueError("Source changed while the work item was running")
            for target in SHARED_TARGETS if share else ():
                publish_target(ARTIFACTS / "canonical" / target, cache / target)
        finally:
            try:
                result = subprocess.run(["docker", "ps", "--all", "--no-trunc", "--filter", "name=^/" + re.escape(name) + "$",
                    "--filter", "label=vibeface.task-root=" + str(ROOT), "--format", "{{.ID}}"], capture_output=True, text=True, check=True, timeout=20)
                container_id = result.stdout.strip()
                if container_id:
                    if len(container_id) != 64 or any(character not in "0123456789abcdef" for character in container_id):
                        raise ValueError("Delivery sandbox cleanup requires one full owned container ID")
                    subprocess.run(["docker", "stop", "--time", "10", container_id], capture_output=True, check=True, timeout=30)
            finally:
                signal.signal(signal.SIGTERM, previous_term)
    except BaseException as error:
        failure = error
    finally:
        try:
            if public is not None:
                finish_host_public(public, small, commit, failure)
        finally:
            if previous_deadline is None:
                os.environ.pop(DEADLINE_ENV, None)
            else:
                os.environ[DEADLINE_ENV] = previous_deadline
    if failure is not None:
        raise failure


def prepare_canonical(fixtures=None):
    if any(name in os.environ for name in ("RISC0_DEV_MODE", "RISC0_SKIP_BUILD")):
        raise ValueError("Delivery verification forbids dev mode and skipped guest builds")
    if (platform.system(), platform.machine(), str(ROOT)) != ("Linux", "x86_64", "/work") or os.environ.get("CARGO_HOME") != "/cargo" or os.environ.get("RISC0_HOME") != "/risc0":
        raise ValueError("Run full delivery verification in the canonical Linux sandbox at /work with CARGO_HOME=/cargo and RISC0_HOME=/risc0")
    manifest, proofs = fixtures or (None, [])
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    environment = {**os.environ, "RISC0_BUILD_LOCKED": "1"}
    target = Path("/artifacts/proof-target")
    run(["cargo", "fmt", "--manifest-path", "crates/delivery/proof/Cargo.toml", "--all", "--", "--check"], env=environment)
    run(["cargo", "fmt", "--manifest-path", "crates/delivery/proof/guest/Cargo.toml", "--", "--check"], env=environment)
    run(["bash", "crates/delivery/proof/build.sh"], env=environment)
    prover = target / "release/vibeface-delivery-prover"
    compiled = json.loads(run([str(prover), "info"], capture_output=True, text=True, env=environment).stdout)
    if fixtures:
        checked_metadata(manifest, proofs, compiled)
    (ARTIFACTS / "host-info.json").write_text(json.dumps(compiled, indent=2) + "\n")
    profile = load_profile(compiled)
    vectors = ARTIFACTS / "public-vectors.json"
    with vectors.open("wb") as output:
        run(["cargo", "run", "--locked", "-p", "vibeface-delivery", "--example", "fixture", "--target-dir", "/artifacts/domain-target", "--", "public", "35", "2", compiled["imageId"], str(PROFILE)], stdout=output, env=environment)
    checked_profile_vectors(json.loads(vectors.read_bytes()), profile, compiled, proofs)
    return compiled, profile, json.loads(vectors.read_bytes()), environment


def check():
    fixtures = optional_fixtures()
    manifest, proofs = fixtures or (None, [])
    compiled, profile, public_vectors, environment = prepare_canonical(fixtures)
    vectors = ARTIFACTS / "public-vectors.json"
    prover = Path("/artifacts/proof-target/release/vibeface-delivery-prover")
    verified = []
    for relation, proof in enumerate(proofs, 1):
        receipt = read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024)
        command = [str(prover), "verify", proof["journal"]]
        result = json.loads(run(command, input=receipt, capture_output=True, env=environment).stdout)
        checked_verification(result, compiled, proof)
        journal = bytearray(hex_bytes(proof["journal"]))
        journal[5 * 32] ^= 1
        rejected = subprocess.run([str(prover), "verify", journal.hex()], input=receipt, capture_output=True, env=environment, cwd=ROOT)
        if rejected.returncode == 0:
            raise ValueError("A receipt accepted a journal with another guest image")
        verified.append({"relation": relation, "journalHash": proof["journalHash"], "verified": True, "wrongJournalRejected": True})
        print(f"PASS current guest relation {relation} public receipt and wrong-journal rejection", flush=True)
    scripts = [("install.mjs", []), ("compile.mjs", ["--check-trust"]), ("exercise-settlement.mjs", ["--self-test"]), ("run.mjs", []), ("verify-risc0.mjs", []), ("compare-vectors.mjs", [str(vectors)])]
    if fixtures:
        scripts.append(("verify-delivery.mjs", [str(FIXTURES)]))
    for script, arguments in scripts:
        run(["node", f"contracts/delivery/{script}", *arguments])
    (ARTIFACTS / "proof-results.json").write_text(json.dumps({"imageId": compiled["imageId"], "schemaHash": compiled["schemaHash"], "receipts": verified}, indent=2) + "\n")
    print("Delivery receipts, cross-language bytes and production EVM adapter checks passed" if fixtures else "Guest rebuild, cross-language bytes and production EVM adapter checks passed; skipped: no public proof fixture")


def object_keys(value, fields):
    if not isinstance(value, dict) or set(value) != set(fields):
        raise ValueError("Unexpected public object fields")


def integer(value, minimum=0, maximum=2 ** 53 - 1):
    if type(value) is not int or not minimum <= value <= maximum:
        raise ValueError("Invalid public integer")


def decimal(value):
    if not isinstance(value, str) or not re.fullmatch(r"0|[1-9][0-9]{0,77}", value):
        raise ValueError("Invalid public decimal amount")


def prefixed(value, size):
    if not isinstance(value, str) or not re.fullmatch("0x[0-9a-f]{" + str(size * 2) + "}", value):
        raise ValueError("Invalid public address or chain hash")


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode()


def read_json_bytes(data):
    def unique(pairs):
        value = {}
        for key, item in pairs:
            if key in value:
                raise ValueError("Duplicate public JSON field")
            value[key] = item
        return value
    def nonfinite(_):
        raise ValueError("Nonfinite public number")
    return json.loads(data, object_pairs_hook=unique, parse_constant=nonfinite)


def exact_tree(directory, expected):
    if any(parent.is_symlink() for parent in (directory, *directory.parents)) or not directory.is_dir():
        raise ValueError("Public input root must be a regular directory")
    found = set()
    directories = {parent.as_posix() for name in expected for parent in Path(name).parents if str(parent) != "."}
    for file in directory.rglob("*"):
        name = file.relative_to(directory).as_posix()
        if file.is_symlink():
            raise ValueError("Public input tree contains a symbolic link")
        if file.is_dir():
            if name not in directories:
                raise ValueError("Unexpected public input directory")
        elif file.is_file() and file.stat().st_nlink == 1:
            found.add(name)
        else:
            raise ValueError("Public input tree contains a nonregular or hardlinked file")
    if found != set(expected) or any(directory.joinpath(name).stat().st_size > limit for name, limit in expected.items()):
        raise ValueError("Public input tree has missing, extra or oversized files")


def checked_context(data, compiled, profile_id, commit):
    context = read_json_bytes(data)
    object_keys(context, ("schemaVersion", "sourceCommit", "recipe", "guest", "profileId", "profileSha256", "file", "deployment", "orderId", "expectedJournals"))
    if type(context["schemaVersion"]) is not int or context["schemaVersion"] != 1 or context["recipe"] != "delivery-small-public-test-v1" or context["sourceCommit"] != commit:
        raise ValueError("Unsupported or cross-source continuation context")
    hex_bytes(commit, 20)
    object_keys(context["guest"], ("imageId", "schemaHash", "elfSha256", "statementBytes"))
    if context["guest"] != compiled or context["profileId"] != profile_id or context["profileSha256"] != hashlib.sha256(PROFILE.read_bytes()).hexdigest():
        raise ValueError("Continuation context differs from the current compiled product profile")
    for field in ("imageId", "schemaHash", "elfSha256"):
        hex_bytes(compiled[field], 32)
    if type(compiled["statementBytes"]) is not int or compiled["statementBytes"] != STATEMENT_BYTES:
        raise ValueError("Continuation guest statement schema mismatch")
    object_keys(context["file"], ("bytes", "sha256"))
    expected_file = bytes((index * 37 + 19) % 251 for index in range(4096))
    if type(context["file"]["bytes"]) is not int or context["file"]["bytes"] != 4096 or context["file"]["sha256"] != hashlib.sha256(expected_file).hexdigest():
        raise ValueError("Continuation file differs from the public 4096-byte recipe")
    hex_bytes(context["orderId"], 32)
    deployment = context["deployment"]
    object_keys(deployment, ("chainId", "confirmations", "accounts", "escrow", "asset", "adapter", "verifier", "payer", "provider", "tokenDecimals", "funderNonce", "mining", "genesisBlock", "runtimeCodeHashes", "baselineBlock", "baselineBalances", "deploymentTransactions"))
    for field, expected in (("chainId", 31337), ("confirmations", 2), ("tokenDecimals", 6), ("funderNonce", 2)):
        if type(deployment[field]) is not int or deployment[field] != expected:
            raise ValueError("Continuation deployment recipe mismatch")
    if not isinstance(deployment["accounts"], list) or len(deployment["accounts"]) != 4 or len(set(deployment["accounts"])) != 4:
        raise ValueError("Continuation requires four distinct public test accounts")
    for address in [*deployment["accounts"], *[deployment[field] for field in ("escrow", "asset", "adapter", "verifier", "payer", "provider")]]:
        prefixed(address, 20)
    if deployment["payer"] != deployment["accounts"][0] or deployment["provider"] != deployment["accounts"][1]:
        raise ValueError("Continuation account ordering mismatch")
    mining = deployment["mining"]
    object_keys(mining, ("genesisTimestamp", "timestampIncrement", "blockTime", "gasPriceWei", "coinbase"))
    if mining != {"genesisTimestamp": 1700000000, "timestampIncrement": 1, "blockTime": 0, "gasPriceWei": "2000000000", "coinbase": "0x" + "0" * 40} or any(type(mining[field]) is not int for field in ("genesisTimestamp", "timestampIncrement", "blockTime")):
        raise ValueError("Continuation mining recipe mismatch")
    for field, number, timestamp in (("genesisBlock", 0, 1700000000), ("baselineBlock", 4, 1700000004)):
        block = deployment[field]
        object_keys(block, ("number", "hash", "timestamp"))
        if type(block["number"]) is not int or block["number"] != number or type(block["timestamp"]) is not int or block["timestamp"] != timestamp:
            raise ValueError("Continuation deployment block mismatch")
        prefixed(block["hash"], 32)
    object_keys(deployment["runtimeCodeHashes"], ("escrow", "asset", "adapter", "verifier"))
    for value in deployment["runtimeCodeHashes"].values():
        prefixed(value, 32)
    balances = deployment["baselineBalances"]
    object_keys(balances, ("nativeWei", "tokenAtoms"))
    for values in balances.values():
        object_keys(values, ("payer", "provider", "escrow"))
        for value in values.values():
            decimal(value)
    if balances["tokenAtoms"] != {"payer": "1000000000000", "provider": "0", "escrow": "0"}:
        raise ValueError("Continuation token baseline mismatch")
    object_keys(deployment["deploymentTransactions"], ("asset", "escrow"))
    for field, block, nonce in (("asset", 1, 0), ("escrow", 3, 1)):
        transaction = deployment["deploymentTransactions"][field]
        object_keys(transaction, ("hash", "blockNumber", "blockHash", "gasUsed", "nonce"))
        for key in ("hash", "blockHash"):
            prefixed(transaction[key], 32)
        decimal(transaction["gasUsed"])
        if type(transaction["blockNumber"]) is not int or transaction["blockNumber"] != block or type(transaction["nonce"]) is not int or transaction["nonce"] != nonce:
            raise ValueError("Continuation deployment transaction ordering mismatch")
    if not isinstance(context["expectedJournals"], list) or len(context["expectedJournals"]) != 4:
        raise ValueError("Continuation requires exactly four expected journals")
    for relation, entry in enumerate(context["expectedJournals"], 1):
        object_keys(entry, ("relation", "journal", "journalHash"))
        checked_public({"mode": "prove", "kind": "groth16", "imageId": compiled["imageId"], "profileId": profile_id, "seal": "00" * 260, **entry}, relation, compiled["imageId"])
    if data != json_bytes(context):
        raise ValueError("Continuation context must preserve canonical JSON bytes")
    return context


def checked_counters(value, complete=False):
    required = {"memory.max", "memory.swap.max", "memory.current", "memory.events", "pids.max", "pids.current"} if complete else set()
    allowed = {"cpu.stat", "memory.max", "memory.swap.max", "memory.current", "memory.peak", "memory.events", "pids.max", "pids.current", "pids.peak"}
    if not isinstance(value, dict) or not required <= set(value) <= allowed:
        raise ValueError("Unexpected cgroup counter fields")
    for name, text in value.items():
        pattern = r"[a-z_]+(?:\.[a-z_]+)* [0-9]+(?:\n[a-z_]+(?:\.[a-z_]+)* [0-9]+)*" if name in ("cpu.stat", "memory.events") else r"[0-9]+"
        if not isinstance(text, str) or len(text) > 4096 or not re.fullmatch(pattern, text):
            raise ValueError("Invalid public cgroup counters")
    if complete and any(value[name] != expected for name, expected in (("memory.max", str(11 * 1024 ** 3)), ("memory.swap.max", "0"), ("pids.max", "64"))):
        raise ValueError("Incorrect bounded sandbox limits")


def checked_resources(value, require_pass=False):
    object_keys(value, ("schemaVersion", "status", "scope", "initial", "final", "oomDelta"))
    if type(value["schemaVersion"]) is not int or value["schemaVersion"] != 1 or value["scope"] != RESOURCE_SCOPE or value["status"] not in ("PASS", "FAIL", "UNKNOWN"):
        raise ValueError("Unexpected work-item resource report")
    if value["initial"] is not None:
        checked_counters(value["initial"], complete=True)
    if value["status"] != "UNKNOWN":
        checked_counters(value["final"], complete=True)
        if value["initial"] is None:
            raise ValueError("Resource report lacks its initial observation")
        delta = oom_delta(value["initial"], value["final"])
        object_keys(value["oomDelta"], ("oom", "oom_kill", "oom_group_kill"))
        for count in value["oomDelta"].values():
            integer(count)
        if value["oomDelta"] != delta or (value["status"] == "PASS") != (not any(delta.values())):
            raise ValueError("Work-item resource status disagrees with kernel OOM counters")
    elif value["final"] is not None or value["oomDelta"] is not None:
        raise ValueError("Unknown resource report must not claim final observations")
    if require_pass and value["status"] != "PASS":
        raise ValueError("A qualified part requires successful final kernel observations")


def safe_identifier(value):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,96}", value):
        raise ValueError("Unexpected public operation identifier")


def checked_process(value):
    object_keys(value, ("label", "exitCode", "elapsedMs", "sandboxCpuUsec", "before", "after", "stderrSha256", "diagnosticMarkers", "succinctCheckpointBytes"))
    safe_identifier(value["label"])
    if value["exitCode"] is not None:
        integer(value["exitCode"], 0, 255)
    for field in ("elapsedMs", "sandboxCpuUsec"):
        if type(value[field]) not in (int, float) or not 0 <= value[field] <= 2 ** 53 - 1:
            raise ValueError("Invalid public process resource measurement")
    for field in ("before", "after"):
        checked_counters(value[field])
    hex_bytes(value["stderrSha256"], 32)
    integer(value["succinctCheckpointBytes"])
    if not isinstance(value["diagnosticMarkers"], list) or not set(value["diagnosticMarkers"]) <= {"memory", "processLimit", "cycles", "panic", "compression"}:
        raise ValueError("Unexpected public diagnostic marker")


def checked_state(value):
    object_keys(value, ("block", "earned", "withdrawn", "refunded", "phase", "decryptionProven", "capsuleBytes", "token", "nativeWei"))
    object_keys(value["block"], ("number", "hash", "timestamp"))
    for name in ("number", "timestamp"):
        integer(value["block"][name])
    prefixed(value["block"]["hash"], 32)
    for name in ("earned", "withdrawn", "refunded", "phase"):
        decimal(value[name])
    if type(value["decryptionProven"]) is not bool:
        raise ValueError("Unexpected public decryption state")
    integer(value["capsuleBytes"], 0, 80)
    object_keys(value["token"], ("payer", "provider", "escrow"))
    for amount in value["token"].values():
        decimal(amount)
    object_keys(value["nativeWei"], ("payer", "provider"))
    for amount in value["nativeWei"].values():
        if not isinstance(amount, str) or not re.fullmatch(r"0x[0-9a-f]{1,64}", amount):
            raise ValueError("Unexpected public native balance")


def checked_branch(value):
    name = value.get("name")
    extra = {"normal": {"laterRestoredByExperiment"}, "pending-late": {"rejected", "revert", "laterRestoredByExperiment"}, "controlled-reorg": {"removedTransaction", "removedBlock", "newCanonicalBlock", "absoluteFinality"}}
    if name not in extra:
        raise ValueError("Unexpected financial experiment branch")
    object_keys(value, {"name", "held", "final", "recovery"} | extra[name])
    for field in ("held", "final", *(["rejected"] if name == "pending-late" else [])):
        checked_state(value[field])
    if name != "controlled-reorg" and value["laterRestoredByExperiment"] is not True or name == "pending-late" and value["revert"] != "Deadline()":
        raise ValueError("Missing branch restoration or intended revert")
    if name == "controlled-reorg":
        for field in ("removedTransaction", "removedBlock", "newCanonicalBlock"):
            prefixed(value[field], 32)
        if value["absoluteFinality"] is not False:
            raise ValueError("Controlled rollback cannot claim absolute finality")
    recovery = value["recovery"]
    object_keys(recovery, ("transactionHash", "wasPending", "capsuleSha256", "bytes", "sha256", "inputs", "claim"))
    prefixed(recovery["transactionHash"], 32)
    for field in ("capsuleSha256", "sha256"):
        hex_bytes(recovery[field], 32)
    if type(recovery["wasPending"]) is not bool or type(recovery["bytes"]) is not int or recovery["bytes"] != 4096 or recovery["inputs"] != ["Hold witness", "observed transaction E"] or recovery["claim"] != "separate offline recovery program receives only two input paths; no OS access or network sandbox claimed":
        raise ValueError("Unexpected safe recovery projection")


def checked_transaction(value):
    object_keys(value, ("label", "hash", "blockHash", "blockNumber", "status", "gasUsed", "effectiveGasPrice", "nativeCostWei", "events"))
    safe_identifier(value["label"])
    for field in ("hash", "blockHash"):
        prefixed(value[field], 32)
    integer(value["blockNumber"])
    integer(value["status"], 0, 1)
    for field in ("gasUsed", "effectiveGasPrice", "nativeCostWei"):
        decimal(value[field])
    event_types = {"KeyUpdated": ("hash", "amount", "hash", "bool"), "Funded": ("hash", "address", "amount"), "WindowUpdated": ("hash", "amount", "amount"), "DecryptionProven": ("hash", "amount"), "Refunded": ("hash", "address", "amount"),
        "BudgetOpened": ("hash", "address", "address", "amount"), "BudgetReserved": ("hash", "hash", "amount"), "BudgetEarned": ("hash", "hash", "amount", "amount"),
        "BudgetWithdrawn": ("hash", "address", "amount"), "BudgetClosed": ("hash",), "BudgetRefunded": ("hash", "address", "amount")}
    if not isinstance(value["events"], list) or len(value["events"]) > 8:
        raise ValueError("Unexpected public event list")
    for event in value["events"]:
        object_keys(event, ("name", "args"))
        kinds = event_types.get(event["name"])
        if kinds is None or not isinstance(event["args"], list) or len(event["args"]) != len(kinds):
            raise ValueError("Unexpected public event projection")
        for argument, kind in zip(event["args"], kinds):
            if kind == "hash":
                prefixed(argument, 32)
            elif kind == "address":
                if not isinstance(argument, str) or not re.fullmatch(r"0x[0-9a-fA-F]{40}", argument):
                    raise ValueError("Unexpected public event address")
            elif kind == "bool":
                if type(argument) is not bool:
                    raise ValueError("Unexpected public event flag")
            else:
                decimal(argument)


def checked_node_summary(value, item, context, commit):
    object_keys(value, ("schemaVersion", "sourceCommit", "workItem", "status", "stage", "guest", "file", "contextSha256", "executionControls", "legacyImagePreTermsRejected", "wrongJournals", "reused", "replayChecks", "proofs", "branches", "transactions", "processes", "limits", "resourceScope", "coverage", "deployment", "corruptERejectedWithoutOutput", "failure"))
    if type(value["schemaVersion"]) is not int or value["schemaVersion"] != 1 or value["sourceCommit"] != commit or value["workItem"] != item or value["status"] not in ("RUNNING", "FAIL", "PREPARED", "PROOF_VERIFIED", "PASS-accept-candidate"):
        raise ValueError("Unexpected Node work-item summary")
    safe_identifier(value["stage"])
    if context is not None:
        if value["guest"] != context["guest"] or value["file"] not in (None, context["file"]) or value["contextSha256"] not in (None, hashlib.sha256(json_bytes(context)).hexdigest()):
            raise ValueError("Node summary uses a different continuation context")
    elif value["file"] is not None or value["contextSha256"] is not None:
        raise ValueError("Node summary claims a missing context")
    if value["limits"] != {"memoryBytes": 11 * 1024 ** 3, "swapBytes": 0, "pids": 64} or value["resourceScope"] != "CPU deltas cover each entire sandbox interval; memory.peak is cumulative sandbox lifetime, not per-process RSS" or value["coverage"] != "synthetic raw 4096-byte file, deterministic public test keys and controlled Ganache branches; no product Node or durable-publication-policy coverage; raw late/reorg controls intentionally exceed its gate; no UI, mobile, real asset, or public-chain finality":
        raise ValueError("Unexpected experiment limits or coverage claim")
    if any(type(limit) is not int for limit in value["limits"].values()):
        raise ValueError("Invalid experiment limit type")
    for field in ("legacyImagePreTermsRejected", "reused", "corruptERejectedWithoutOutput"):
        if type(value[field]) is not bool:
            raise ValueError("Unexpected work-item control flag")
    integer(value["replayChecks"], 0, 2)
    for field, limit in (("executionControls", 4), ("wrongJournals", 4), ("proofs", 4), ("branches", 3), ("transactions", 32), ("processes", 96)):
        if not isinstance(value[field], list) or len(value[field]) > limit:
            raise ValueError("Unexpected work-item observation count")
    for relation, control in enumerate(value["executionControls"], 1):
        object_keys(control, ("relation", "cycles", "segments", "journalHash", "executed", "proved", "originalJournalDecoderEof", "wrongJournalDecoderEof"))
        if type(control["relation"]) is not int or control["relation"] != relation or any(control[field] is not expected for field, expected in (("executed", True), ("proved", False), ("originalJournalDecoderEof", True), ("wrongJournalDecoderEof", True))):
            raise ValueError("Execution and decoder controls are incomplete")
        integer(control["cycles"], 1, PROFILE_CAPS["max_guest_cycles"])
        integer(control["segments"], 1)
        if context is None or control["journalHash"] != context["expectedJournals"][relation - 1]["journalHash"]:
            raise ValueError("Execute control journal differs from context")
    for relation, wrong in enumerate(value["wrongJournals"], 1):
        object_keys(wrong, ("relation", "journal"))
        hex_bytes(wrong["journal"], STATEMENT_BYTES)
        if type(wrong["relation"]) is not int or wrong["relation"] != relation or context is None or wrong["journal"] == context["expectedJournals"][relation - 1]["journal"]:
            raise ValueError("Missing intended wrong-journal control")
    seen = set()
    for proof in value["proofs"]:
        object_keys(proof, ("relation", "cycles", "segments", "receiptBytes", "sealBytes", "journalHash", "independentlyVerified", "wrongJournalRejected", "reused"))
        integer(proof["relation"], 1, 4)
        if proof["relation"] in seen:
            raise ValueError("Duplicate Node proof observation")
        seen.add(proof["relation"])
        integer(proof["cycles"], 1, PROFILE_CAPS["max_guest_cycles"])
        integer(proof["segments"], 1)
        integer(proof["receiptBytes"], 1, 16 * 1024 * 1024)
        if proof["sealBytes"] != 260 or proof["independentlyVerified"] is not True or proof["wrongJournalRejected"] is not True or type(proof["reused"]) is not bool or context is None or proof["journalHash"] != context["expectedJournals"][proof["relation"] - 1]["journalHash"]:
            raise ValueError("Incomplete Node proof verification observation")
    for record in value["processes"]:
        checked_process(record)
    for record in value["branches"]:
        checked_branch(record)
    for record in value["transactions"]:
        checked_transaction(record)
    if value["deployment"] is not None:
        deployment = value["deployment"]
        object_keys(deployment, ("chainId", "escrow", "asset", "payer", "provider", "profileId", "adapter", "verifier", "tokenDecimals", "orderId", "budgetId", "allocationTransaction", "runtimeCodeHashes"))
        if context is None or any(deployment[field] != context["deployment"][field] for field in ("chainId", "escrow", "asset", "payer", "provider", "adapter", "verifier", "tokenDecimals", "runtimeCodeHashes")) or deployment["profileId"] != "0x" + context["profileId"] or deployment["orderId"] != "0x" + context["orderId"]:
            raise ValueError("Final deployment differs from reconstructed context")
        for field in ("budgetId", "allocationTransaction"):
            prefixed(deployment[field], 32)
    if value["failure"] is not None:
        object_keys(value["failure"], ("stage", "kind", "code"))
        for field in ("stage", "kind", "code"):
            if value["failure"][field] is not None:
                safe_identifier(value["failure"][field])
    return value


def checked_producer(value):
    object_keys(value, ("repository", "workflowPath", "event", "runId", "runAttempt", "headSha"))
    if not isinstance(value["repository"], str) or not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", value["repository"]) or value["workflowPath"] != WORKFLOW_PATH or value["event"] != "workflow_dispatch":
        raise ValueError("Unexpected Actions producer identity")
    integer(value["runId"], 1)
    integer(value["runAttempt"], 1)
    hex_bytes(value["headSha"], 20)


def current_producer(commit):
    if os.environ.get("GITHUB_ACTIONS") != "true":
        return None
    value = {"repository": os.environ["GITHUB_REPOSITORY"], "workflowPath": WORKFLOW_PATH, "event": os.environ["GITHUB_EVENT_NAME"],
             "runId": int(os.environ["GITHUB_RUN_ID"]), "runAttempt": int(os.environ["GITHUB_RUN_ATTEMPT"]), "headSha": os.environ["GITHUB_SHA"]}
    checked_producer(value)
    if value["headSha"] != commit or not os.environ.get("GITHUB_WORKFLOW_REF", "").startswith(value["repository"] + "/" + WORKFLOW_PATH + "@"):
        raise ValueError("Actions source does not match the current clean checkout")
    return value


def gh_api(repository, suffix, binary=False):
    result = subprocess.run(["gh", "api", "repos/" + repository + "/actions/" + suffix], capture_output=True, check=True, timeout=min(60, remaining() or 60))
    return result.stdout if binary else read_json_bytes(result.stdout)


def checked_run_api(value, producer, run_id, attempt=None):
    if value["id"] != run_id or value["path"] != WORKFLOW_PATH or value["event"] != "workflow_dispatch" or value["head_sha"] != producer["headSha"] or value["repository"]["full_name"] != producer["repository"] or value["head_repository"]["full_name"] != producer["repository"]:
        raise ValueError("Resume workflow API identity differs from this source and repository")
    integer(value["run_attempt"], 1)
    if attempt is not None and value["run_attempt"] != attempt:
        raise ValueError("Artifact claims another producer attempt")


def checked_origin(value):
    object_keys(value, ("repository", "workflowPath", "event", "runId", "runAttempt", "headSha", "artifactId", "artifactName", "artifactDigest"))
    checked_producer({key: value[key] for key in ("repository", "workflowPath", "event", "runId", "runAttempt", "headSha")})
    integer(value["artifactId"], 1)
    if not isinstance(value["artifactDigest"], str) or not re.fullmatch(r"sha256:[0-9a-f]{64}", value["artifactDigest"]):
        raise ValueError("Actions artifact lacks an immutable SHA-256 digest")
    match = re.fullmatch(r"delivery-small-(prepare|relation-[1-4]|accept)-([0-9a-f]{40})-([1-9][0-9]*)-([1-9][0-9]*)", value["artifactName"])
    if match is None or match.group(2) != value["headSha"] or int(match.group(3)) != value["runId"] or int(match.group(4)) != value["runAttempt"]:
        raise ValueError("Unexpected immutable artifact name")
    return match.group(1)


def artifact_origin(artifact, producer, run_id):
    from datetime import datetime, timezone
    name = artifact["name"]
    match = re.fullmatch(r"delivery-small-(prepare|relation-[1-4]|accept)-([0-9a-f]{40})-([1-9][0-9]*)-([1-9][0-9]*)", name)
    if match is None:
        raise ValueError("Unexpected small-experiment artifact name")
    value = {**producer, "runId": run_id, "runAttempt": int(match.group(4)), "artifactId": artifact["id"], "artifactName": name, "artifactDigest": artifact["digest"]}
    item = checked_origin(value)
    integer(artifact["size_in_bytes"], 1, (70 if item == "accept" else 20) * 1024 * 1024)
    if artifact["expired"] is not False or datetime.fromisoformat(artifact["expires_at"].replace("Z", "+00:00")) <= datetime.now(timezone.utc) or artifact["workflow_run"]["id"] != run_id or artifact["workflow_run"]["head_sha"] != producer["headSha"]:
        raise ValueError("Expired or cross-source Actions artifact")
    attempt = gh_api(producer["repository"], f"runs/{run_id}/attempts/{value['runAttempt']}")
    checked_run_api(attempt, producer, run_id, value["runAttempt"])
    created = datetime.fromisoformat(artifact["created_at"].replace("Z", "+00:00"))
    started = datetime.fromisoformat(attempt["run_started_at"].replace("Z", "+00:00"))
    if created < started:
        raise ValueError("Artifact predates its claimed producer attempt")
    return value


def public_layout(item, publication, has_context=True):
    expected = {"summary.json": 2 * 1024 * 1024, "resources.json": 64 * 1024}
    if has_context:
        expected["context.json"] = 64 * 1024
    if item.startswith("relation-") and publication in ("VERIFIED_PARTIAL", "UNQUALIFIED"):
        prefix = "partials/" + item + "/"
        expected[prefix + item + ".json"] = 32 * 1024
        expected[prefix + item + ".receipt"] = 16 * 1024 * 1024
        if publication == "VERIFIED_PARTIAL":
            expected[prefix + "part.json"] = 64 * 1024
    elif item == "accept" and publication == "VERIFIED_BUNDLE":
        expected.update({"proofs/" + name: 16 * 1024 if name == "manifest.json" else 32 * 1024 if name.endswith(".json") else 16 * 1024 * 1024 for name in PROOF_NAMES})
    elif publication != "NO_PROOF":
        raise ValueError("Unexpected public publication kind for work item")
    return expected


def checked_export_summary(value, item, commit):
    object_keys(value, ("schemaVersion", "sourceCommit", "workItem", "status", "publication", "controlOrigin", "contextSha256", "producer", "inputArtifacts", "node", "failureCode"))
    if type(value["schemaVersion"]) is not int or value["schemaVersion"] != 1 or value["sourceCommit"] != commit or value["workItem"] != item or value["status"] not in ("PREPARED", "VERIFIED_PARTIAL", "PASS", "FAIL") or value["publication"] not in ("NO_PROOF", "VERIFIED_PARTIAL", "UNQUALIFIED", "VERIFIED_BUNDLE") or value["controlOrigin"] not in ("actions", "local-control"):
        raise ValueError("Unexpected public work-item summary")
    if value["producer"] is not None:
        checked_producer(value["producer"])
        if value["producer"]["headSha"] != commit or value["controlOrigin"] != "actions":
            raise ValueError("Public work-item source provenance mismatch")
    elif value["controlOrigin"] != "local-control" or item != "prepare":
        raise ValueError("Only prepare controls may omit Actions provenance")
    valid_success = {"prepare": ("PREPARED", "NO_PROOF"), "accept": ("PASS", "VERIFIED_BUNDLE")}
    expected = valid_success.get(item, ("VERIFIED_PARTIAL", "VERIFIED_PARTIAL"))
    if value["status"] != "FAIL" and (value["status"], value["publication"]) != expected or value["publication"] == "VERIFIED_PARTIAL" and value["status"] != "VERIFIED_PARTIAL" or value["publication"] == "UNQUALIFIED" and value["status"] != "FAIL":
        raise ValueError("Work-item status disagrees with its publication state")
    if value["contextSha256"] is not None:
        hex_bytes(value["contextSha256"], 32)
    if value["failureCode"] is not None:
        safe_identifier(value["failureCode"])
    if not isinstance(value["inputArtifacts"], list) or len(value["inputArtifacts"]) > 64:
        raise ValueError("Unexpected public import provenance count")
    ids = set()
    for origin in value["inputArtifacts"]:
        checked_origin(origin)
        if origin["artifactId"] in ids or origin["headSha"] != commit or value["producer"] is not None and origin["repository"] != value["producer"]["repository"]:
            raise ValueError("Duplicate or cross-source public artifact origin")
        ids.add(origin["artifactId"])
    return value


def extract_artifact(data, origin, destination):
    if len(data) > 20 * 1024 * 1024 or "sha256:" + hashlib.sha256(data).hexdigest() != origin["artifactDigest"]:
        raise ValueError("Actions artifact bytes differ from their immutable digest")
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        entries = archive.infolist()
        if not entries or len(entries) > 16 or sum(entry.file_size for entry in entries) > 20 * 1024 * 1024:
            raise ValueError("Oversized public artifact archive")
        names = [entry.filename for entry in entries]
        if len(set(names)) != len(names):
            raise ValueError("Duplicate public archive paths")
        for entry in entries:
            name = entry.filename
            mode = entry.external_attr >> 16
            if name.startswith("/") or "\\" in name or any(part in ("", ".", "..") for part in name.split("/")) or entry.is_dir() or stat.S_IFMT(mode) not in (0, stat.S_IFREG):
                raise ValueError("Nonregular or unsafe public archive path")
        summary_entry = archive.getinfo("summary.json")
        if summary_entry.file_size > 2 * 1024 * 1024:
            raise ValueError("Oversized artifact summary")
        summary = checked_export_summary(read_json_bytes(archive.read(summary_entry)), checked_origin(origin), origin["headSha"])
        expected = public_layout(summary["workItem"], summary["publication"], "context.json" in names)
        if set(names) != set(expected) or any(entry.file_size > expected[entry.filename] for entry in entries):
            raise ValueError("Artifact has missing, extra or oversized public files")
        destination.mkdir(mode=0o700)
        for entry in entries:
            file = destination / entry.filename
            file.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            with file.open("xb") as output:
                output.write(archive.read(entry))
            file.chmod(0o600)
        exact_tree(destination, expected)
    return summary


def download_inputs(producer, resume_run):
    checked_producer(producer)
    run_ids = [producer["runId"]]
    if resume_run is not None:
        integer(resume_run, 1)
        if resume_run not in run_ids:
            run_ids.append(resume_run)
    current = gh_api(producer["repository"], f"runs/{producer['runId']}/attempts/{producer['runAttempt']}")
    checked_run_api(current, producer, producer["runId"], producer["runAttempt"])
    incoming = ARTIFACTS / "canonical/incoming"
    incoming.mkdir(mode=0o700, parents=True, exist_ok=True)
    directory = incoming / uuid.uuid4().hex
    directory.mkdir(mode=0o700)
    origins, seen = [], {}
    for run_id in run_ids:
        checked_run_api(gh_api(producer["repository"], f"runs/{run_id}"), producer, run_id)
        page = 1
        while True:
            artifacts = gh_api(producer["repository"], f"runs/{run_id}/artifacts?per_page=100&page={page}")["artifacts"]
            for artifact in artifacts:
                if not artifact["name"].startswith("delivery-small-"):
                    continue
                origin = artifact_origin(artifact, producer, run_id)
                if checked_origin(origin) == "accept":
                    continue
                previous = seen.get(origin["artifactId"])
                if previous is not None:
                    if previous != origin:
                        raise ValueError("Conflicting immutable artifact identity")
                    continue
                seen[origin["artifactId"]] = origin
                if len(seen) > 64:
                    raise ValueError("Too many continuation artifact candidates")
                summary = extract_artifact(gh_api(producer["repository"], f"artifacts/{origin['artifactId']}/zip", binary=True), origin, directory / str(origin["artifactId"]))
                if summary["producer"] != {key: origin[key] for key in producer}:
                    raise ValueError("Artifact upload provenance differs from the Actions API")
                if summary["publication"] == "VERIFIED_PARTIAL":
                    relation_item = summary["workItem"]
                    part = read_json_bytes(read_fixture("part.json", 64 * 1024, directory / str(origin["artifactId"]) / "partials" / relation_item))
                    original = part["producerArtifact"]
                    if original is not None:
                        checked_origin(original)
                        if original["repository"] != producer["repository"] or original["headSha"] != producer["headSha"]:
                            raise ValueError("Reused proof producer is outside this source and repository")
                        actual = artifact_origin(gh_api(producer["repository"], f"artifacts/{original['artifactId']}"), producer, original["runId"])
                        if actual != original:
                            raise ValueError("Original proof artifact identity changed")
                origins.append(origin)
            if len(artifacts) < 100:
                break
            page += 1
    with (directory / "index.json").open("xb") as output:
        output.write(json_bytes({"schemaVersion": 1, "sourceCommit": producer["headSha"], "repository": producer["repository"], "artifacts": sorted(origins, key=lambda origin: origin["artifactId"])}))
    (directory / "index.json").chmod(0o600)
    return directory


def verify_pair(directory, relation, context, compiled, wrong_journal=None):
    proof = read_json_bytes(read_fixture(f"relation-{relation}.json", 32 * 1024, directory))
    checked_public(proof, relation, compiled["imageId"])
    expected = context["expectedJournals"][relation - 1]
    if proof["profileId"] != context["profileId"] or any(proof[field] != expected[field] for field in ("relation", "journal", "journalHash")):
        raise ValueError("Public proof does not bind the reconstructed order and product profile")
    receipt = read_fixture(f"relation-{relation}.receipt", 16 * 1024 * 1024, directory)
    if not receipt:
        raise ValueError("An empty receipt is not proof evidence")
    prover = "/artifacts/proof-target/release/vibeface-delivery-prover"
    result = read_json_bytes(run([prover, "verify", proof["journal"]], input=receipt, capture_output=True).stdout)
    checked_verification(result, compiled, proof)
    if wrong_journal is not None:
        hex_bytes(wrong_journal, STATEMENT_BYTES)
        if wrong_journal == proof["journal"]:
            raise ValueError("Wrong-journal control repeats the original statement")
        rejected = subprocess.run([prover, "verify", wrong_journal], input=receipt, capture_output=True, cwd=ROOT, timeout=remaining())
        if rejected.returncode <= 0 or rejected.stderr.strip() != b"Error: receipt journal mismatch":
            raise ValueError("Public proof did not reject the intended wrong journal")
    return proof


def checked_part(part, directory, context, origin):
    object_keys(part, ("schemaVersion", "status", "sourceCommit", "contextSha256", "relation", "journalHash", "publicSha256", "receiptSha256", "wrongJournal", "producer", "producerArtifact", "provingResources", "verificationResources"))
    integer(part["relation"], 1, 4)
    relation = part["relation"]
    if type(part["schemaVersion"]) is not int or part["schemaVersion"] != 1 or part["status"] != "VERIFIED_PARTIAL" or part["sourceCommit"] != context["sourceCommit"] or part["contextSha256"] != hashlib.sha256(json_bytes(context)).hexdigest() or part["journalHash"] != context["expectedJournals"][relation - 1]["journalHash"]:
        raise ValueError("Qualified part differs from this source, context or journal")
    checked_producer(part["producer"])
    producer_origin = part["producerArtifact"] or origin
    if checked_origin(producer_origin) != f"relation-{relation}" or part["producer"] != {key: producer_origin[key] for key in part["producer"]} or part["producer"]["headSha"] != context["sourceCommit"] or producer_origin["repository"] != origin["repository"]:
        raise ValueError("Qualified part original producer provenance mismatch")
    for suffix, key, limit in (("json", "publicSha256", 32 * 1024), ("receipt", "receiptSha256", 16 * 1024 * 1024)):
        hex_bytes(part[key], 32)
        if hashlib.sha256(read_fixture(f"relation-{relation}.{suffix}", limit, directory)).hexdigest() != part[key]:
            raise ValueError("Qualified part public bytes differ from their certificate")
    hex_bytes(part["wrongJournal"], STATEMENT_BYTES)
    if part["wrongJournal"] == context["expectedJournals"][relation - 1]["journal"]:
        raise ValueError("Qualified part lacks an intended negative journal")
    object_keys(part["provingResources"], ("sandbox", "process"))
    checked_resources(part["provingResources"]["sandbox"], require_pass=True)
    checked_resources(part["verificationResources"], require_pass=True)
    checked_process(part["provingResources"]["process"])
    if part["provingResources"]["process"]["label"] != f"prove-{relation}" or part["provingResources"]["process"]["exitCode"] != 0:
        raise ValueError("Qualified part lacks the original successful proving measurement")
    return part


def normalize_inputs(directory, destination, compiled, vectors, commit, item, repository):
    if directory is None:
        if item != "prepare":
            raise ValueError("This work item requires prepared public continuation inputs")
        return None, {}, []
    index = read_json_bytes(read_fixture("index.json", 64 * 1024, directory))
    object_keys(index, ("schemaVersion", "sourceCommit", "repository", "artifacts"))
    if type(index["schemaVersion"]) is not int or index["schemaVersion"] != 1 or index["sourceCommit"] != commit or index["repository"] != repository or not isinstance(index["artifacts"], list) or len(index["artifacts"]) > 64:
        raise ValueError("Unsupported continuation candidate index")
    ids = set()
    context, context_data = None, None
    candidates = {relation: [] for relation in range(1, 5)}
    expected_root = {"index.json": 64 * 1024}
    for origin in index["artifacts"]:
        origin_item = checked_origin(origin)
        if origin["artifactId"] in ids or origin["headSha"] != commit or origin["repository"] != index["repository"] or origin_item == "accept":
            raise ValueError("Duplicate, cross-source or unexpected continuation artifact")
        ids.add(origin["artifactId"])
        artifact = directory / str(origin["artifactId"])
        summary = checked_export_summary(read_json_bytes(read_fixture("summary.json", 2 * 1024 * 1024, artifact)), origin_item, commit)
        if summary["producer"] != {key: origin[key] for key in ("repository", "workflowPath", "event", "runId", "runAttempt", "headSha")}:
            raise ValueError("Candidate upload producer differs from its authoritative origin")
        layout = public_layout(origin_item, summary["publication"], (artifact / "context.json").exists())
        exact_tree(artifact, layout)
        expected_root.update({str(origin["artifactId"]) + "/" + name: limit for name, limit in layout.items()})
        resources = read_json_bytes(read_fixture("resources.json", 64 * 1024, artifact))
        checked_resources(resources)
        if "context.json" not in layout:
            if summary["publication"] != "NO_PROOF":
                raise ValueError("Claimed proof candidate lacks context")
            continue
        data = read_fixture("context.json", 64 * 1024, artifact)
        value = checked_context(data, compiled, vectors["profileId"], commit)
        if summary["contextSha256"] != hashlib.sha256(data).hexdigest() or context_data is not None and data != context_data:
            raise ValueError("Conflicting continuation contexts")
        context, context_data = value, data
        if summary["node"] is not None:
            checked_node_summary(summary["node"], origin_item, context, commit)
        if summary["publication"] == "VERIFIED_PARTIAL":
            if summary["status"] != "VERIFIED_PARTIAL":
                raise ValueError("Unqualified result claims a reusable certificate")
            checked_resources(resources, require_pass=True)
            relation = int(origin_item[-1])
            pair = artifact / "partials" / origin_item
            part = checked_part(read_json_bytes(read_fixture("part.json", 64 * 1024, pair)), pair, context, origin)
            if part["relation"] != relation:
                raise ValueError("Artifact contains a certificate for another relation")
            verify_pair(pair, relation, context, compiled, part["wrongJournal"])
            candidates[relation].append((origin, pair, part))
    exact_tree(directory, expected_root)
    if context is None:
        if item != "prepare":
            raise ValueError("No validated same-source prepared context is available")
        return None, {}, index["artifacts"]
    selected = {relation: sorted(values, key=lambda candidate: (candidate[0]["runId"], candidate[0]["runAttempt"], candidate[0]["artifactId"]))[0] for relation, values in candidates.items() if values}
    if item == "accept" and set(selected) != {1, 2, 3, 4}:
        missing = ",".join(str(relation) for relation in range(1, 5) if relation not in selected)
        raise ValueError("Missing qualified public proof parts: " + missing)
    destination.mkdir(mode=0o700)
    (destination / "context.json").write_bytes(context_data)
    (destination / "context.json").chmod(0o600)
    if item == "accept":
        proofs = destination / "proofs"
        proofs.mkdir(mode=0o700)
        manifest = {"schemaVersion": 1, **{field: compiled[field] for field in ("imageId", "schemaHash", "elfSha256")},
                    "proofs": [{"receipt": f"relation-{relation}.receipt", "public": f"relation-{relation}.json"} for relation in range(1, 5)]}
        (proofs / "manifest.json").write_bytes(json_bytes(manifest))
        for relation, (_, pair, _) in selected.items():
            for suffix in ("json", "receipt"):
                name = f"relation-{relation}.{suffix}"
                (proofs / name).write_bytes(read_fixture(name, 32 * 1024 if suffix == "json" else 16 * 1024 * 1024, pair))
        exact_tree(proofs, {name: 16 * 1024 if name == "manifest.json" else 32 * 1024 if name.endswith(".json") else 16 * 1024 * 1024 for name in PROOF_NAMES})
        load_fixtures(proofs)
    elif item.startswith("relation-") and int(item[-1]) in selected:
        _, source, _ = selected[int(item[-1])]
        pair = destination / "partials" / item
        pair.mkdir(mode=0o700, parents=True)
        for suffix in ("json", "receipt"):
            name = item + "." + suffix
            (pair / name).write_bytes(read_fixture(name, 32 * 1024 if suffix == "json" else 16 * 1024 * 1024, source))
    for file in destination.rglob("*"):
        file.chmod(0o700 if file.is_dir() else 0o600)
    return context, selected, index["artifacts"]


def publish_small_results(public, summary, resources, context_data=None, pair=None, part=None, verified_bundle=None):
    checked_export_summary(summary, summary["workItem"], summary["sourceCommit"])
    checked_resources(resources)
    if part is not None and (pair is None or verified_bundle is not None or summary["publication"] != "VERIFIED_PARTIAL" or summary["status"] != "VERIFIED_PARTIAL" or resources["status"] != "PASS"):
        raise ValueError("Only a qualified successful part can publish a reusable certificate")
    if summary["publication"] == "VERIFIED_PARTIAL" and part is None or summary["publication"] == "UNQUALIFIED" and (pair is None or part is not None) or summary["publication"] == "VERIFIED_BUNDLE" and verified_bundle is None:
        raise ValueError("Publication state disagrees with independently verified evidence")
    if pair is not None and verified_bundle is not None:
        raise ValueError("Partial and complete proof publication must remain separate")
    base = public.parent
    if any(parent.is_symlink() for parent in (base, *base.parents)):
        raise ValueError("Public experiment ancestors must not be symlinks")
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    for directory in (base.parent, base):
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError("Public experiment directories must not be symlinks")
    temporary = base / (".public-" + uuid.uuid4().hex)
    temporary.mkdir(mode=0o700)
    outputs = {"summary.json": json_bytes(summary), "resources.json": json_bytes(resources)}
    if context_data is not None:
        outputs["context.json"] = context_data
    if pair is not None:
        item = summary["workItem"]
        for suffix in ("json", "receipt"):
            outputs["partials/" + item + "/" + item + "." + suffix] = read_fixture(item + "." + suffix, 32 * 1024 if suffix == "json" else 16 * 1024 * 1024, pair)
        if part is not None:
            outputs["partials/" + item + "/part.json"] = json_bytes(part)
    if verified_bundle is not None:
        exact_tree(verified_bundle, {name: 16 * 1024 if name == "manifest.json" else 32 * 1024 if name.endswith(".json") else 16 * 1024 * 1024 for name in PROOF_NAMES})
        for name in PROOF_NAMES:
            outputs["proofs/" + name] = read_fixture(name, 16 * 1024 if name == "manifest.json" else 32 * 1024 if name.endswith(".json") else 16 * 1024 * 1024, verified_bundle)
    expected = public_layout(summary["workItem"], summary["publication"], context_data is not None)
    if set(outputs) != set(expected) or any(len(data) > expected[name] for name, data in outputs.items()):
        raise ValueError("Unsafe public export layout or size")
    for name, data in outputs.items():
        destination = temporary / name
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        with destination.open("xb") as output:
            output.write(data)
        destination.chmod(0o644)
    exact_tree(temporary, expected)
    for directory in temporary.rglob("*"):
        if directory.is_dir():
            directory.chmod(0o755)
    temporary.chmod(0o755)
    if public.exists() or public.is_symlink():
        if public.is_symlink() or not public.is_dir():
            raise ValueError("Unsafe public experiment destination")
        private = base / "private"
        if private.is_symlink():
            raise ValueError("Unsafe private experiment directory")
        private.mkdir(mode=0o700, exist_ok=True)
        saved = private / ("node-public-" + uuid.uuid4().hex)
        public.rename(saved)
        for file in saved.rglob("*"):
            if file.is_symlink() or not file.is_dir() and (not file.is_file() or file.stat().st_nlink != 1):
                raise ValueError("Unsafe original public candidate")
            file.chmod(0o700 if file.is_dir() else 0o600)
        saved.chmod(0o700)
    temporary.rename(public)
    for directory in (base.parent, base):
        directory.chmod(0o755)


def base_summary(item, commit, producer):
    return {"schemaVersion": 1, "sourceCommit": commit, "workItem": item, "status": "FAIL", "publication": "NO_PROOF",
            "controlOrigin": "actions" if producer is not None else "local-control", "contextSha256": None, "producer": producer,
            "inputArtifacts": [], "node": None, "failureCode": None}


def unknown_resources(initial=None):
    return {"schemaVersion": 1, "status": "UNKNOWN", "scope": RESOURCE_SCOPE, "initial": initial, "final": None, "oomDelta": None}


def finish_host_public(public, item, commit, failure):
    invalid = False
    try:
        summary = checked_export_summary(read_json_bytes(read_fixture("summary.json", 2 * 1024 * 1024, public)), item, commit)
        resources = read_json_bytes(read_fixture("resources.json", 64 * 1024, public))
        checked_resources(resources)
        context_data = read_fixture("context.json", 64 * 1024, public) if (public / "context.json").exists() else None
        exact_tree(public, public_layout(item, summary["publication"], context_data is not None))
        if context_data is not None:
            context = read_json_bytes(context_data)
            checked_context(context_data, context["guest"], context["profileId"], commit)
            load_profile(context["guest"])
            if summary["contextSha256"] != hashlib.sha256(context_data).hexdigest():
                raise ValueError("Published context digest mismatch")
            if summary["node"] is not None:
                checked_node_summary(summary["node"], item, context, commit)
    except (ValueError, OSError, KeyError, TypeError):
        invalid = True
        producer = current_producer(commit)
        summary, resources, context_data = base_summary(item, commit, producer), unknown_resources(), None
        summary["failureCode"] = "HOST_WORK_ITEM_FAILED"
        public = public.parent.parent / ("host-" + uuid.uuid4().hex) / "public"
        publish_small_results(public, summary, resources)
    else:
        if failure is not None and summary["status"] != "FAIL":
            summary.update(status="FAIL", failureCode="HOST_OR_CONTAINER_CLEANUP_FAILED")
            pair = public / "partials" / item if summary["publication"] == "VERIFIED_PARTIAL" else None
            bundle = public / "proofs" if summary["publication"] == "VERIFIED_BUNDLE" else None
            if pair is not None:
                summary["publication"] = "UNQUALIFIED"
            public = public.parent.parent / ("host-" + uuid.uuid4().hex) / "public"
            publish_small_results(public, summary, resources, context_data, pair=pair, verified_bundle=bundle)
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with open(output_file, "a") as output:
            output.write("public-directory=" + public.relative_to(ROOT).as_posix() + "\n")
            if item == "prepare" and failure is None and summary["status"] == "PREPARED" and context_data is not None:
                context = read_json_bytes(context_data)
                matrix = {"item": [f"relation-{entry['relation']}" for entry in context["expectedJournals"]]}
                output.write("matrix=" + json.dumps(matrix, separators=(",", ":")) + "\n")
    if failure is None and (invalid or summary["status"] == "FAIL"):
        raise ValueError("Canonical work item did not publish a qualified successful result")
    return public


def prove_small(item, resume_dir=None):
    if item not in ITEMS:
        raise ValueError("Unsupported delivery work item")
    remaining()
    if resume_dir is not None:
        if not re.fullmatch(r"/artifacts/incoming/[0-9a-f]{32}", str(resume_dir)) or any(parent.is_symlink() for parent in (resume_dir, *resume_dir.parents)):
            raise ValueError("Internal continuation inputs must stay in the existing canonical mount")
    commit = os.environ.get("VIBEFACE_DELIVERY_SOURCE_COMMIT")
    hex_bytes(commit, 20)
    if DEADLINE_ENV not in os.environ:
        raise ValueError("Continuation requires the host's source and absolute deadline")
    producer = read_json_bytes(os.environ.get("VIBEFACE_DELIVERY_PRODUCER", "null"))
    if producer is not None:
        checked_producer(producer)
        if producer["headSha"] != commit:
            raise ValueError("Continuation producer source mismatch")
    elif item != "prepare" or resume_dir is not None:
        raise ValueError("Only local prepare controls may omit Actions provenance")
    work_id = os.environ.get("VIBEFACE_DELIVERY_WORK_ID", uuid.uuid4().hex)
    if not re.fullmatch(r"[0-9a-f]{32}", work_id):
        raise ValueError("Invalid fresh work-item identifier")
    destination = CANONICAL / "small" / work_id
    destination.parent.mkdir(parents=True, exist_ok=True)
    public = destination / "public"
    normalized = destination.parent / ("inputs-" + work_id)
    summary = base_summary(item, commit, producer)
    initial = None
    resources, context, compiled, selected = unknown_resources(), None, None, {}
    failure, deferred_node_failure, pair, bundle, part = None, None, None, None, None
    try:
        _, initial = cgroup_resources()
        compiled, _, vectors, _ = prepare_canonical()
        for script, args in (("install.mjs", []), ("compile.mjs", ["--check-trust"])):
            run(["node", "contracts/delivery/" + script, *args])
        context, selected, origins = normalize_inputs(resume_dir, normalized, compiled, vectors, commit, item, producer["repository"] if producer else None)
        summary["inputArtifacts"] = origins
        command = ["node", "contracts/delivery/experiment-delivery.mjs", item, str(destination)]
        if context is not None:
            command.append(str(normalized))
        try:
            run(command)
        except Exception as error:
            deferred_node_failure = error
        context_data = read_fixture("context.json", 64 * 1024, public)
        current = checked_context(context_data, compiled, vectors["profileId"], commit)
        if context is not None and json_bytes(context) != context_data:
            raise ValueError("Node reconstructed a different continuation context")
        context = current
        summary["contextSha256"] = hashlib.sha256(context_data).hexdigest()
        node = checked_node_summary(read_json_bytes(read_fixture("summary.json", 2 * 1024 * 1024, public)), item, context, commit)
        summary["node"] = node
        directory = destination / "private/proofs"
        if item.startswith("relation-"):
            relation = int(item[-1])
            exact_tree(directory, {item + ".json": 32 * 1024, item + ".receipt": 16 * 1024 * 1024})
            verify_pair(directory, relation, context, compiled)
            pair = directory
            summary["publication"] = "UNQUALIFIED"
        if deferred_node_failure is not None:
            raise deferred_node_failure
        if len(node["executionControls"]) != 4 or len(node["wrongJournals"]) != 4 or node["legacyImagePreTermsRejected"] is not True or node["failure"] is not None:
            raise ValueError("Work item did not finish all actual execute and parser controls")
        proves = [process for process in node["processes"] if process["label"].startswith("prove-")]
        if item == "prepare":
            exact_tree(directory, {})
            if node["status"] != "PREPARED" or node["replayChecks"] != 2 or proves or node["proofs"] or node["reused"]:
                raise ValueError("Prepare did not finish two real deterministic replay controls")
            summary["status"] = "PREPARED"
        elif item.startswith("relation-"):
            reused = relation in selected
            if node["status"] != "PROOF_VERIFIED" or node["replayChecks"] != 0 or node["reused"] is not reused or len(node["proofs"]) != 1 or node["proofs"][0]["relation"] != relation or node["proofs"][0]["reused"] is not reused or len(proves) != (0 if reused else 1) or not reused and (proves[0]["label"] != f"prove-{relation}" or proves[0]["exitCode"] != 0):
                raise ValueError("A relation item must prove only its selected relation or verify a reused pair")
            wrong = node["wrongJournals"][relation - 1]["journal"]
            verify_pair(pair, relation, context, compiled, wrong)
            if reused and selected[relation][2]["wrongJournal"] != wrong:
                raise ValueError("Reused proof negative control differs from the current Node constructor")
            summary["status"] = "VERIFIED_PARTIAL"
        else:
            if set(selected) != {1, 2, 3, 4} or proves or node["status"] != "PASS-accept-candidate" or node["replayChecks"] != 1 or len(node["proofs"]) != 4 or len(node["branches"]) != 3 or {branch["name"] for branch in node["branches"]} != {"normal", "pending-late", "controlled-reorg"} or node["corruptERejectedWithoutOutput"] is not True:
                raise ValueError("Accept did not complete the original proof, money and recovery experiment")
            exact_tree(directory, {name: 16 * 1024 if name == "manifest.json" else 32 * 1024 if name.endswith(".json") else 16 * 1024 * 1024 for name in PROOF_NAMES})
            manifest, proofs = load_fixtures(directory)
            checked_metadata(manifest, proofs, compiled)
            checked_profile_vectors(vectors, load_profile(compiled), compiled, proofs)
            for relation in range(1, 5):
                verify_pair(directory, relation, context, compiled, node["wrongJournals"][relation - 1]["journal"])
            bundle = directory
            summary.update(status="PASS", publication="VERIFIED_BUNDLE")
    except Exception as error:
        failure = error
        summary.update(status="FAIL", failureCode="WORK_ITEM_FAILED")
    finally:
        if item.startswith("relation-") and pair is None and context is not None and compiled is not None:
            try:
                candidate = destination / "private/proofs"
                exact_tree(candidate, {item + ".json": 32 * 1024, item + ".receipt": 16 * 1024 * 1024})
                verify_pair(candidate, int(item[-1]), context, compiled)
                pair = candidate
                summary["publication"] = "UNQUALIFIED"
                summary["contextSha256"] = hashlib.sha256(json_bytes(context)).hexdigest()
            except Exception:
                pass
        try:
            _, final = cgroup_resources()
            delta = oom_delta(initial, final)
            resources = {"schemaVersion": 1, "status": "FAIL" if any(delta.values()) else "PASS", "scope": RESOURCE_SCOPE, "initial": initial, "final": final, "oomDelta": delta}
            checked_resources(resources)
        except Exception:
            resources = unknown_resources(initial)
        if resources["status"] != "PASS":
            summary.update(status="FAIL", failureCode="RESOURCE_OBSERVATION_FAILED")
            failure = failure or ValueError("Work-item final resources were unknown or incurred OOM events")
        if summary["status"] == "VERIFIED_PARTIAL":
            if reused:
                origin, _, original = selected[relation]
                part = {**original, "producerArtifact": original["producerArtifact"] or origin, "verificationResources": resources}
            else:
                part = {"schemaVersion": 1, "status": "VERIFIED_PARTIAL", "sourceCommit": commit, "contextSha256": summary["contextSha256"],
                        "relation": relation, "journalHash": context["expectedJournals"][relation - 1]["journalHash"],
                        "publicSha256": hashlib.sha256(read_fixture(item + ".json", 32 * 1024, pair)).hexdigest(),
                        "receiptSha256": hashlib.sha256(read_fixture(item + ".receipt", 16 * 1024 * 1024, pair)).hexdigest(),
                        "wrongJournal": wrong, "producer": producer, "producerArtifact": None,
                        "provingResources": {"sandbox": resources, "process": proves[0]}, "verificationResources": resources}
            summary["publication"] = "VERIFIED_PARTIAL"
        context_data = json_bytes(context) if context is not None else None
        publish_small_results(public, summary, resources, context_data, pair, part, bundle)
        print(f"Small work item {item} {summary['status']}: {public}", flush=True)
    if failure is not None:
        raise ValueError("Delivery work item failed; only independently verified public evidence was retained") from None



def self_test():
    from tests.delivery.test_fixture_check import run
    return run(sys.modules.get(__name__))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--self-test", action="store_true")
    mode.add_argument("--check-fixtures", action="store_true")
    mode.add_argument("--prove-small", choices=ITEMS, metavar="ITEM")
    parser.add_argument("--sandbox", action="store_true")
    parser.add_argument("--resume-run", type=int, help="explicit prior same-source Actions run; host only")
    parser.add_argument("--resume-dir", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.resume_run is not None and (not args.sandbox or args.prove_small is None or args.resume_run <= 0):
        parser.error("--resume-run requires --sandbox --prove-small ITEM and a positive run ID")
    if args.resume_dir is not None and (args.sandbox or args.prove_small is None or args.resume_run is not None):
        parser.error("--resume-dir is internal to one canonical work item")
    if args.self_test:
        from unittest.mock import patch
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix="shared-cache-self-test-", dir=ARTIFACTS) as cache, patch(f"{__name__}.shared_cache", return_value=Path(cache)):
            self_test()
    elif args.check_fixtures:
        if optional_fixtures():
            print("Public fixture structure and schema checked; proof verification has not run")
    elif args.sandbox:
        sandbox(small=args.prove_small, resume_run=args.resume_run)
    elif args.prove_small:
        prove_small(args.prove_small, args.resume_dir)
    else:
        check()
