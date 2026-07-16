#!/usr/bin/env python3
"""Bootstrap validator for the implementation-neutral fixture corpus (task T00-01).

Runs the four checks required by the T00-01 card:
  1. schema validation  — every data file validates against its JSON Schema in schema/
  2. ID uniqueness      — fixture case ids + corpus query ids are globally unique
  3. runner dry-run     — every fixture + referenced input artifact loads and is counted
  4. no backend fields  — no denylisted vector-store payload key appears anywhere

It depends only on the Python 3 standard library. If the reference `jsonschema` package is
installed it is used for authoritative Draft 2020-12 validation; otherwise a built-in subset
validator (covering exactly the features these schemas use) is applied. Exit code 0 = green.

This is a temporary bootstrap: T00-03's Rust harness will validate the same schemas and consume
the same fixtures.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

FIXTURES = Path(__file__).resolve().parent.parent
SCHEMA_DIR = FIXTURES / "schema"

# Vector-store / backend payload keys that MUST NOT appear as a key in any fixture (01 §7, 14 §1).
DENYLIST = {
    "file_path", "chunk_type", "code_vector", "description_vector", "named_vector",
    "collection", "payload", "parent_id", "children_ids", "is_parent", "file_hash",
    "branches", "qdrant", "vector", "points", "upsert", "prefetch",
    "memory_type", "expires_at", "content_hash",
}

# Data file -> schema file.
DOC_SCHEMAS = {
    "manifest.json": "manifest.schema.json",
    "search/corpus.json": "corpus.schema.json",
    "fault/matrix.json": "fault-matrix.schema.json",
    "reconcile/index.json": "case-index.schema.json",
    "memory/index.json": "case-index.schema.json",
    "adversarial/index.json": "case-index.schema.json",
    "fault/index.json": "case-index.schema.json",
}

CASE_INDEX_FILES = ["reconcile/index.json", "memory/index.json",
                    "adversarial/index.json", "fault/index.json"]

errors: list[str] = []
notes: list[str] = []


def fail(msg: str) -> None:
    errors.append(msg)


def load_json(rel: str):
    path = FIXTURES / rel
    if not path.exists():
        fail(f"missing file: {rel}")
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        fail(f"{rel}: invalid JSON — {exc}")
        return None


# ── Built-in JSON Schema subset validator ───────────────────────────────────────

_TYPE_CHECKS = {
    "object": lambda v: isinstance(v, dict),
    "array": lambda v: isinstance(v, list),
    "string": lambda v: isinstance(v, str),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "null": lambda v: v is None,
}


def _resolve_ref(ref: str, root: dict):
    assert ref.startswith("#/"), f"only internal refs supported: {ref}"
    node = root
    for part in ref[2:].split("/"):
        node = node[part]
    return node


def _subset_validate(inst, schema: dict, root: dict, path: str, out: list[str]) -> None:
    if "$ref" in schema:
        schema = _resolve_ref(schema["$ref"], root)

    if "const" in schema and inst != schema["const"]:
        out.append(f"{path}: expected const {schema['const']!r}, got {inst!r}")
    if "enum" in schema and inst not in schema["enum"]:
        out.append(f"{path}: {inst!r} not in enum {schema['enum']}")

    t = schema.get("type")
    if t is not None:
        types = t if isinstance(t, list) else [t]
        if not any(_TYPE_CHECKS[tt](inst) for tt in types):
            out.append(f"{path}: expected type {t}, got {type(inst).__name__}")
            return  # further checks assume the type held

    if isinstance(inst, str):
        if "minLength" in schema and len(inst) < schema["minLength"]:
            out.append(f"{path}: shorter than minLength {schema['minLength']}")
        pat = schema.get("pattern")
        if pat is not None and re.search(pat, inst) is None:
            out.append(f"{path}: {inst!r} does not match pattern {pat!r}")

    if isinstance(inst, list):
        if "minItems" in schema and len(inst) < schema["minItems"]:
            out.append(f"{path}: fewer than minItems {schema['minItems']} (got {len(inst)})")
        if "maxItems" in schema and len(inst) > schema["maxItems"]:
            out.append(f"{path}: more than maxItems {schema['maxItems']} (got {len(inst)})")
        item_schema = schema.get("items")
        if isinstance(item_schema, dict):
            for i, el in enumerate(inst):
                _subset_validate(el, item_schema, root, f"{path}[{i}]", out)

    if isinstance(inst, dict):
        for req in schema.get("required", []):
            if req not in inst:
                out.append(f"{path}: missing required property '{req}'")
        props = schema.get("properties", {})
        addl = schema.get("additionalProperties", True)
        for key, val in inst.items():
            child = f"{path}.{key}"
            if key in props:
                _subset_validate(val, props[key], root, child, out)
            elif isinstance(addl, dict):
                _subset_validate(val, addl, root, child, out)
            elif addl is False:
                out.append(f"{child}: additional property not allowed")


def validate_doc(rel: str, schema_rel: str, data) -> None:
    schema = load_json(f"schema/{schema_rel}")
    if schema is None or data is None:
        return
    try:
        import jsonschema  # type: ignore
        validator = jsonschema.Draft202012Validator(schema)
        found = sorted(validator.iter_errors(data), key=lambda e: list(e.path))
        for e in found:
            loc = "/".join(str(p) for p in e.path) or "<root>"
            fail(f"{rel} [{loc}]: {e.message}")
    except ImportError:
        out: list[str] = []
        _subset_validate(data, schema, schema, rel, out)
        errors.extend(out)


# ── Denylist scan (keys only) ───────────────────────────────────────────────────

def scan_denylist(rel: str, node, path: str) -> None:
    if isinstance(node, dict):
        for key, val in node.items():
            if key.lower() in DENYLIST:
                fail(f"{rel}: denylisted backend key '{key}' at {path}")
            scan_denylist(rel, val, f"{path}.{key}")
    elif isinstance(node, list):
        for i, el in enumerate(node):
            scan_denylist(rel, el, f"{path}[{i}]")


# ── Main ────────────────────────────────────────────────────────────────────────

def main() -> int:
    docs = {rel: load_json(rel) for rel in DOC_SCHEMAS}

    engine = "jsonschema" if _has_jsonschema() else "built-in subset validator"
    print(f"[validate] fixtures root : {FIXTURES}")
    print(f"[validate] schema engine : {engine}")

    # 1. schema validation
    for rel, schema_rel in DOC_SCHEMAS.items():
        validate_doc(rel, schema_rel, docs.get(rel))

    # 4. denylist scan (over every data file, keys only)
    for rel, data in docs.items():
        if data is not None:
            scan_denylist(rel, data, rel)

    # 2. ID uniqueness + 3. dry-run counting
    seen: dict[str, str] = {}
    counts: dict[str, int] = {}

    def register(scope: str, _id, where: str) -> None:
        if not isinstance(_id, str):
            fail(f"{where}: id must be a string, got {_id!r}")
            return
        if _id in seen:
            fail(f"duplicate id '{_id}' in {where} (first seen in {seen[_id]})")
        else:
            seen[_id] = where

    for rel in CASE_INDEX_FILES:
        data = docs.get(rel)
        if not isinstance(data, dict):
            continue
        cases = data.get("cases", [])
        counts[rel] = len(cases)
        for c in cases:
            register("case", c.get("id"), rel)
            # dry-run: resolve any referenced input artifact
            inp = c.get("input", {})
            if isinstance(inp, dict) and isinstance(inp.get("input_path"), str):
                if not (FIXTURES / inp["input_path"]).exists():
                    fail(f"{rel}: case '{c.get('id')}' input_path missing: {inp['input_path']}")

    corpus = docs.get("search/corpus.json")
    if isinstance(corpus, dict):
        queries = corpus.get("queries", [])
        counts["search/corpus.json"] = len(queries)
        for q in queries:
            register("query", q.get("id"), "search/corpus.json")

    # matrix row ids unique (separate namespace)
    matrix = docs.get("fault/matrix.json")
    if isinstance(matrix, dict):
        row_seen: set[str] = set()
        total_rows = 0
        for mx in matrix.get("matrices", []):
            for row in mx.get("rows", []):
                total_rows += 1
                rid = row.get("id")
                if rid in row_seen:
                    fail(f"fault/matrix.json: duplicate matrix row id '{rid}'")
                row_seen.add(rid)
        counts["fault/matrix.json (rows)"] = total_rows

    # manifest cross-checks
    manifest = docs.get("manifest.json")
    if isinstance(manifest, dict):
        fams = manifest.get("families", [])
        names = [f.get("name") for f in fams]
        expected_fams = {"parser", "reconcile", "search", "memory", "adversarial", "fault"}
        if set(names) != expected_fams:
            fail(f"manifest.families must cover exactly {sorted(expected_fams)}, got {sorted(names)}")
        gap_families = {g.get("family") for g in manifest.get("gaps", [])}
        gap_ids = [g.get("id") for g in manifest.get("gaps", [])]
        if len(gap_ids) != len(set(gap_ids)):
            fail("manifest.gaps has duplicate gap ids")
        for f in fams:
            cov, art, fname = f.get("coverage"), f.get("artifact"), f.get("name")
            if cov == "gap-only":
                if art is not None:
                    fail(f"manifest: family '{fname}' is gap-only but has artifact {art!r}")
                if fname not in gap_families:
                    fail(f"manifest: family '{fname}' is gap-only but has no registered gap")
            elif art is not None and not (FIXTURES / art).exists():
                fail(f"manifest: family '{fname}' artifact missing on disk: {art}")
        # thresholds must all be TBD
        thresholds = manifest.get("baseline", {}).get("thresholds", {})
        for k, v in thresholds.items():
            if v != "TBD":
                fail(f"manifest.baseline.thresholds['{k}'] must be 'TBD' (O2), got {v!r}")

    # ── report ──
    print("[validate] fixture counts:")
    for rel in sorted(counts):
        print(f"  - {rel:32s} {counts[rel]}")
    print(f"[validate] unique fixture/query ids: {len(seen)}")

    if errors:
        print(f"\n[validate] FAILED with {len(errors)} problem(s):")
        for e in errors:
            print(f"  ✗ {e}")
        return 1

    print("\n[validate] OK — schema valid, ids unique, dry-run loaded, no backend keys.")
    return 0


def _has_jsonschema() -> bool:
    try:
        import jsonschema  # noqa: F401
        return True
    except ImportError:
        return False


if __name__ == "__main__":
    sys.exit(main())
