#!/usr/bin/env python3
"""Generate a conservative Shopify CLI command inventory from an upstream checkout."""
from __future__ import annotations
import argparse, datetime, json, pathlib, re, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
JSON_OUT = ROOT / "inventory/shopify-cli.json"
MD_OUT = ROOT / "inventory/PARITY.md"
COMMAND_MARKER = "/src/cli/commands/"
CONFIG_RE = re.compile(r"['\"]([^'\"]+(?:\.toml|\.json|\.yaml|\.yml))['\"]")
ENV_RE = re.compile(r"\b(?:env\s*:\s*['\"]?|process\.env\.)([A-Z][A-Z0-9_]*)")
FLAG_RE = re.compile(r"^\s+['\"]?([a-z][a-z0-9-]*)['\"]?\s*:\s*(?:Flags\.|[a-zA-Z][\w]*Flag\b)", re.M)
CHAR_RE = re.compile(r"\bchar\s*:\s*['\"]([^'\"]+)['\"]")
EXECUTABLES = ("node", "npm", "pnpm", "yarn", "bun", "esbuild", "cloudflared", "ruby", "git", "docker")
API_MARKERS = {
    "admin-graphql": ("AdminApi", "admin_graphql", "adminApi", "graphql.json"),
    "partners-graphql": ("Partners", "partnersApi", "partners_graphql"),
    "business-platform-graphql": ("BusinessPlatform", "businessPlatform"),
    "storefront": ("Storefront", "storefrontApi"),
}
ADAPTER_GROUPS = {"hydrogen"}
ADAPTER_COMMANDS = {"app dev", "app build", "app init", "theme check", "theme language-server", "theme dev"}

def git(path: pathlib.Path, *args: str) -> str:
    return subprocess.check_output(["git", "-C", str(path), *args], text=True).strip()

def package_version(upstream: pathlib.Path) -> str:
    return json.loads((upstream / "packages/cli/package.json").read_text())["version"]

def command_files(upstream: pathlib.Path):
    for path in sorted((upstream / "packages").glob("*/src/cli/commands/**/*.ts")):
        if path.name.endswith(".test.ts") or path.name in {"index.ts", "constants.ts"}:
            continue
        yield path

def command_name(path: pathlib.Path) -> str:
    normalized = path.as_posix()
    suffix = normalized.split(COMMAND_MARKER, 1)[1][:-3]
    return suffix.replace("/", " ")

def classify(name: str) -> str:
    if name.split()[0] in ADAPTER_GROUPS or name in ADAPTER_COMMANDS:
        return "adapter-backed"
    return "deferred"

def balanced_block(source: str, start: int, opening: str = "{", closing: str = "}") -> str:
    depth = 0
    quote = None
    escaped = False
    for index in range(start, len(source)):
        char = source[index]
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char in "'\"`":
            quote = char
        elif char == opening:
            depth += 1
        elif char == closing:
            depth -= 1
            if depth == 0:
                return source[start:index + 1]
    return source[start:]

def flag_entries(source: str):
    flags_start = source.find("static flags")
    if flags_start < 0:
        return []
    object_start = source.find("{", flags_start)
    if object_start < 0:
        return []
    block = balanced_block(source, object_start)
    matches = list(FLAG_RE.finditer(block))
    entries = []
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(block)
        declaration = block[match.start():end]
        char = CHAR_RE.search(declaration)
        entries.append({"name": match.group(1), "short": char.group(1) if char else None})
    return entries

def external_executables(source: str) -> list[str]:
    detected = []
    for executable in EXECUTABLES:
        patterns = (
            rf"(?:spawn|exec|run|command)\w*\s*\(\s*['\"]{re.escape(executable)}['\"]",
            rf"\b(?:command|executable|binary|program)\s*:\s*['\"]{re.escape(executable)}['\"]",
            rf"['\"]{re.escape(executable)}(?:\.exe)?['\"]\s*,\s*\[",
        )
        if any(re.search(pattern, source, re.I) for pattern in patterns):
            detected.append(executable)
    return detected

def related_sources(upstream: pathlib.Path, command_path: pathlib.Path, source: str):
    texts = [source]
    for match in re.finditer(r"from\s+['\"](\.{1,2}/[^'\"]+)['\"]", source):
        candidate = (command_path.parent / match.group(1)).resolve()
        for resolved in (candidate, candidate.with_suffix(".ts"), candidate / "index.ts"):
            try: resolved.relative_to(upstream)
            except ValueError: continue
            if resolved.is_file():
                try: texts.append(resolved.read_text())
                except UnicodeDecodeError: pass
                break
    return "\n".join(texts)

def build(upstream: pathlib.Path) -> dict:
    commands = []
    for path in command_files(upstream):
        source = path.read_text()
        related = related_sources(upstream, path, source)
        name = command_name(path)
        flags = flag_entries(source)
        env = sorted(set(ENV_RE.findall(related)))
        configs = sorted(set(CONFIG_RE.findall(related)))
        executables = external_executables(related)
        apis = sorted(name for name, markers in API_MARKERS.items() if any(marker in related for marker in markers))
        aliases = []
        commands.append({
            "name": name,
            "group": name.split()[0],
            "nested_groups": name.split()[:-1],
            "aliases": aliases,
            "flags": flags,
            "environment_variables": env,
            "config_files": configs,
            "external_executables": executables,
            "shopify_apis": apis,
            "classification": classify(name),
            "source": str(path.relative_to(upstream)),
        })
    return {
        "schema_version": 1,
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "upstream": {
            "repository": "https://github.com/Shopify/cli",
            "version": package_version(upstream),
            "commit": git(upstream, "rev-parse", "HEAD"),
            "commit_date": git(upstream, "show", "-s", "--format=%cI", "HEAD"),
            "license": "MIT",
        },
        "limitations": [
            "Static conservative scan; dynamically composed flags and transitive runtime dependencies may be absent.",
            "Aliases are recorded when declared in command metadata; current source scan found none.",
            "API and executable fields are evidence markers, not a complete call graph.",
        ],
        "commands": commands,
    }

def runtime_command_records(payload: list[dict], source: str) -> list[dict]:
    """Normalize `shopify commands --json` output without executing commands."""
    commands = []
    for record in payload:
        command_id = record.get("id")
        if not command_id:
            continue
        name = command_id.replace(":", " ")
        flags = []
        for flag_name, flag in sorted((record.get("flags") or {}).items()):
            flags.append({"name": flag_name, "short": flag.get("char")})
        commands.append({
            "name": name,
            "id": command_id,
            "group": name.split()[0],
            "nested_groups": name.split()[:-1],
            "aliases": sorted(record.get("aliases") or []),
            "flags": flags,
            "environment_variables": sorted({
                flag.get("env") for flag in (record.get("flags") or {}).values()
                if flag.get("env")
            }),
            "config_files": [],
            "external_executables": [],
            "shopify_apis": [],
            "classification": classify(name),
            "summary": record.get("summary", ""),
            "plugin_name": record.get("pluginName"),
            "plugin_type": record.get("pluginType"),
            "hidden": bool(record.get("hidden", False)),
            "provenance": source,
        })
    return commands

def manifest_command_records(manifest: dict, source: str) -> list[dict]:
    records = []
    for command_id, record in sorted((manifest.get("commands") or {}).items()):
        records.append({
            "id": command_id,
            "summary": record.get("description", "").split("\n", 1)[0],
            "aliases": record.get("aliases", []),
            "flags": record.get("flags", {}),
            "hidden": record.get("hidden", False),
            "pluginName": manifest.get("pluginName"),
            "pluginType": manifest.get("pluginType"),
        })
    return runtime_command_records(records, source)

def build_runtime(executable: str) -> dict:
    try:
        output = subprocess.check_output([executable, "commands", "--json"], text=True)
        payload = json.loads(output)
    except (OSError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        raise RuntimeError(f"unable to read runtime command manifest from {executable}: {error}") from error
    version = subprocess.check_output([executable, "--version"], text=True).strip()
    executable_path = pathlib.Path(subprocess.check_output(["realpath", executable], text=True).strip())
    package_root = executable_path.parent.parent
    manifest_paths = sorted(package_root.glob("oclif.manifest.json"))
    manifest_paths.extend(package_root.glob("node_modules/**/oclif.manifest.json"))
    manifest_commands = []
    for path in manifest_paths:
        try:
            manifest_commands.extend(manifest_command_records(json.loads(path.read_text()), str(path)))
        except (OSError, json.JSONDecodeError):
            continue
    return {
        "schema_version": 2,
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "runtime": {"executable": executable, "version": version},
        "manifests": {
            "paths": [str(path) for path in manifest_paths],
            "command_count": len(manifest_commands),
            "commands": manifest_commands,
        },
        "limitations": [
            "Runtime inventory is derived from the installed CLI and is not a reproducible source snapshot.",
            "Commands hidden by the runtime manifest are retained and marked hidden.",
        ],
        "commands": runtime_command_records(payload, "runtime:shopify commands --json"),
    }

def markdown(data: dict) -> str:
    rows = [
        "# Shopify CLI parity inventory",
        "",
        f"Generated from Shopify CLI `{data['upstream']['version']}` at commit [`{data['upstream']['commit'][:12]}`](https://github.com/Shopify/cli/commit/{data['upstream']['commit']}).",
        "",
        "Classifications: `native`, `adapter-backed`, `deferred`, or `unsupported`. This table is generated; edit classifications in the generator policy until implementation metadata is introduced.",
        "",
        "| Command | Aliases | Flags | Env | Config | Executables | APIs | Status |",
        "|---|---|---:|---:|---:|---|---|---|",
    ]
    for command in data["commands"]:
        rows.append("| `{}` | {} | {} | {} | {} | {} | {} | `{}` |".format(
            command["name"], ", ".join(command["aliases"]) or "—", len(command["flags"]),
            len(command["environment_variables"]), len(command["config_files"]),
            ", ".join(command["external_executables"]) or "—", ", ".join(command["shopify_apis"]) or "—",
            command["classification"],
        ))
    rows += ["", "## Scanner limitations", ""] + [f"- {item}" for item in data["limitations"]]
    return "\n".join(rows) + "\n"

def canonical_for_check(data: dict) -> dict:
    data = dict(data); data.pop("generated_at", None); return data

def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("upstream", type=pathlib.Path, nargs="?")
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--runtime", metavar="EXECUTABLE", help="capture an installed Shopify CLI runtime manifest")
    parser.add_argument("--runtime-output", type=pathlib.Path, default=ROOT / "inventory/runtime-shopify-cli.json")
    args = parser.parse_args()
    if args.runtime:
        data = build_runtime(args.runtime)
        rendered = json.dumps(data, indent=2) + "\n"
        if args.check:
            if not args.runtime_output.exists():
                print("runtime inventory is stale; rerun without --check", file=sys.stderr)
                return 1
            existing = json.loads(args.runtime_output.read_text())
            if canonical_for_check(existing) != canonical_for_check(data):
                print("runtime inventory is stale; rerun without --check", file=sys.stderr)
                return 1
            return 0
        args.runtime_output.parent.mkdir(parents=True, exist_ok=True)
        args.runtime_output.write_text(rendered)
        print(f"generated {len(data['commands'])} runtime commands")
        return 0
    if args.upstream is None:
        parser.error("upstream checkout is required unless --runtime is used")
    data = build(args.upstream.resolve())
    rendered_json = json.dumps(data, indent=2) + "\n"
    rendered_md = markdown(data)
    if args.check:
        existing = json.loads(JSON_OUT.read_text())
        if canonical_for_check(existing) != canonical_for_check(data) or MD_OUT.read_text() != rendered_md:
            print("inventory is stale; run scripts/generate-inventory.py against the pinned checkout", file=sys.stderr)
            return 1
        return 0
    JSON_OUT.parent.mkdir(parents=True, exist_ok=True)
    JSON_OUT.write_text(rendered_json)
    MD_OUT.write_text(rendered_md)
    print(f"generated {len(data['commands'])} commands from {data['upstream']['commit']}")
    return 0

if __name__ == "__main__": raise SystemExit(main())
