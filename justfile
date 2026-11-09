default: help

# List the available recipes.
help:
    @just --list

# Build the workspace.
build:
    cargo build -j 2

# Type-check the workspace.
check:
    cargo check --workspace -j 2

# Run the test suite.
test:
    cargo test -j 2

# Run the application.
run *args:
    cargo run -p blazer-chat -j 2 -- {{args}}

# Format the tree.
fmt:
    cargo fmt --all

# Verify formatting (gate).
fmt-check:
    cargo fmt --all --check

# Lint with warnings as errors (gate).
clippy:
    cargo clippy --workspace --all-targets -j 2 -- -D warnings

# Everything a change must pass before it lands.
gate: fmt-check clippy test api-check pins-check
    @echo "gate green"

# Fail the gate if client code calls an endpoint missing from the API
# contract, or if the Swagger page drifted from openapi.json.
api-check:
    #!/usr/bin/env python3
    import json, pathlib, re

    spec = json.loads(pathlib.Path("docs/api/openapi.json").read_text())
    paths = set(spec["paths"])

    found = set()
    for f in pathlib.Path("crates").rglob("*.rs"):
        if "fixtures" in f.parts:
            continue
        found |= set(re.findall(r"/api/v1/[A-Za-z]+(?:\.[A-Za-z]+)*", f.read_text()))
    # History endpoints are router arms built dynamically (kind c/p/d).
    found |= {"/api/v1/channels.history", "/api/v1/groups.history", "/api/v1/im.history"}
    # Static asset routes with rust-format placeholders in the code.
    found |= {"/file-upload/{fileId}/{name}", "/avatar/{username}"}

    def documented(endpoint: str) -> bool:
        return any(
            sp == endpoint or sp.startswith(endpoint + "/") for sp in paths
        ) or any(
            sp.split("{")[0].rstrip("/") == endpoint for sp in paths
        )

    missing = sorted(e for e in found if not documented(e))
    assert not missing, f"endpoints missing from docs/api/openapi.json: {missing}"

    html = pathlib.Path("docs/api/index.html").read_text()
    match = re.search(r"const spec = (\{.*?\});\n", html, re.S)
    assert match, "index.html lost the inlined spec"
    assert json.loads(match.group(1)) == spec, "index.html spec drifted from openapi.json"
    print(f"api-check: {len(found)} endpoints documented, swagger in sync")

# Serve the API contract (Swagger UI) at http://localhost:8321.
swagger:
    python3 -m http.server 8321 --directory docs/api

# Fail the gate if the bezel-gpui crates drift apart: the lib and its
# platform must resolve to one snapshot or runtime bootstrap breaks
# (m10 S5 T6).
pins-check:
    #!/usr/bin/env python3
    import pathlib, re, sys

    lock = pathlib.Path("Cargo.lock").read_text()
    versions = {}
    for name, version in re.findall(
        r'name = "(bezel-gpui[a-z-]*)"\nversion = "([^"]+)"', lock
    ):
        versions.setdefault(name, set()).add(version)
    if not versions:
        sys.exit("no bezel-gpui crates in Cargo.lock")
    drifted = {name: sorted(vs) for name, vs in versions.items() if len(vs) > 1}
    distinct = {sorted(vs)[0] for vs in versions.values()}
    if drifted or len(distinct) > 1:
        sys.exit(f"bezel-gpui family drifted: {drifted or dict(versions)}")
    print(f"pins ok: {len(versions)} bezel crates at {sorted(distinct)[0]}")
