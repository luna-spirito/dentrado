#!/usr/bin/env python3
"""Enforce the intra-crate module reading order.

Each crate's modules are visited depth-first: a module's children are visited
in the order of their `mod` declarations, and the module itself after all of
its children (a postorder).  A module may only refer to modules that occur
earlier in that traversal.

Ancestor and descendant references are structural: a child may use names its
parent re-exports, and a parent may name its children.  Only references
*between branches* (siblings, cousins, ...) constrain the order.  That is what
removes cycles: the module tree itself is acyclic, so any remaining cycle is a
genuine cross-branch import cycle.

Cross-crate imports are Cargo's business and are not checked.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------
# Lexical helpers

_IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


_SPECIAL = re.compile(r"//|/\*|\"|'")


def _raw_prefix(text: str, quote: int) -> int | None:
    """Start of the `r`/`br` prefix if the quote opens a raw string."""
    hash_end = quote - 1
    while hash_end >= 0 and text[hash_end] == "#":
        hash_end -= 1
    if hash_end < 0 or text[hash_end] != "r":
        return None
    start = hash_end
    if start > 0 and text[start - 1] in "bc":
        start -= 1
    if start > 0 and (text[start - 1].isalnum() or text[start - 1] == "_"):
        return None
    return start


def strip_comments_and_strings(text: str) -> str:
    """Replace comments and literals with spaces, preserving offsets."""
    out: list[str] = []
    i, n = 0, len(text)
    while i < n:
        m = _SPECIAL.search(text, i)
        if m is None:
            out.append(text[i:])
            break
        j = m.start()
        tok = m.group()
        if tok == "//":
            out.append(text[i:j])
            end = text.find("\n", j)
            end = n if end < 0 else end
            out.append(" " * (end - j))
            i = end
        elif tok == "/*":
            out.append(text[i:j])
            depth, end = 1, j + 2
            while end < n and depth:
                if text.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif text.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
            out.append(" " * (end - j))
            i = end
        elif tok == '"':
            start = _raw_prefix(text, j)
            if start is not None:
                hashes = j - start - 1
                if text[start] in "bc":
                    hashes -= 1
                out.append(text[i:start])
                close = text.find('"' + "#" * hashes, j + 1)
                end = n if close < 0 else close + 1 + hashes
                out.append(" " * (end - start))
                i = end
            else:
                out.append(text[i:j])
                end = j + 1
                while end < n:
                    if text[end] == "\\":
                        end += 2
                    elif text[end] == '"':
                        end += 1
                        break
                    else:
                        end += 1
                out.append(" " * (end - j))
                i = end
        else:  # `'`
            # A char literal is `'x'` or `'\...'`; anything else is a lifetime.
            if j + 2 < n and text[j + 2] == "'":
                out.append(text[i:j])
                out.append(" " * 3)
                i = j + 3
            elif j + 1 < n and text[j + 1] == "\\":
                out.append(text[i:j])
                end = j + 2
                while end < n and text[end] != "'":
                    end += 1
                end = min(end + 1, n)
                out.append(" " * (end - j))
                i = end
            else:
                out.append(text[i : j + 1])
                i = j + 1
    return "".join(out)


def skip_ws(text: str, i: int) -> int:
    while i < len(text) and text[i].isspace():
        i += 1
    return i


# ---------------------------------------------------------------------------
# Module tree

@dataclass
class Module:
    path: tuple[str, ...]
    file: Path
    scan: str                       # own source, inline child bodies masked
    children: list["Module"] = field(default_factory=list)


def mask(text: str, ranges: list[tuple[int, int]]) -> str:
    """Blank the given spans, preserving line breaks (and therefore offsets)."""
    if not ranges:
        return text
    pieces: list[str] = []
    last = 0
    for start, end in ranges:
        pieces.append(text[last:start])
        pieces.append(re.sub(r"[^\n]", " ", text[start:end]))
        last = end
    pieces.append(text[last:])
    return "".join(pieces)


@dataclass
class Decl:
    name: str
    start: int
    end: int
    body: tuple[int, int] | None    # absolute (start, end) of inline body


def module_decls(masked: str) -> list[Decl]:
    """Top-level `mod` declarations of an already-stripped source file."""
    decls: list[Decl] = []
    depth, i, n = 0, 0, len(masked)
    while i < n:
        c = masked[i]
        if c == "{":
            depth, i = depth + 1, i + 1
            continue
        if c == "}":
            depth, i = depth - 1, i + 1
            continue
        if depth == 0 and masked.startswith("mod", i):
            before = masked[i - 1] if i else " "
            after = masked[i + 3] if i + 3 < n else " "
            if not (before.isalnum() or before == "_") and not (
                after.isalnum() or after == "_"
            ):
                j = skip_ws(masked, i + 3)
                m = _IDENT.match(masked, j)
                if not m:
                    i += 3
                    continue
                name, k = m.group(0), skip_ws(masked, m.end())
                if k >= n or masked[k] not in ";{":
                    i = k
                    continue
                if masked[k] == ";":
                    decls.append(Decl(name, i, k + 1, None))
                    i = k + 1
                    continue
                depth2, e = 1, k + 1
                while e < n and depth2:
                    if masked[e] == "{":
                        depth2 += 1
                    elif masked[e] == "}":
                        depth2 -= 1
                    e += 1
                decls.append(Decl(name, i, e, (k + 1, e - 1)))
                i = e
                continue
        i += 1
    return decls


def attributes_before(text: str, pos: int) -> str:
    """The attribute/doc-comment block immediately above `pos`."""
    start = text.rfind("\n", 0, pos) + 1
    while start > 0:
        prev_end = start - 1
        line_start = text.rfind("\n", 0, prev_end) + 1
        line = text[line_start:prev_end].strip()
        if line.startswith("#") or line.startswith("///") or line.startswith("//!"):
            start = line_start
        else:
            break
    return text[start:pos]


def is_test_only(attrs: str) -> bool:
    return re.search(r"#\s*\[\s*cfg\s*\([^)]*\btest\b", attrs) is not None


def path_attr(attrs: str) -> str | None:
    m = re.search(r'#\s*\[\s*path\s*=\s*"([^"]+)"\s*\]', attrs)
    return m.group(1) if m else None


def parse_tree(file: Path, path: tuple[str, ...] = ()) -> Module:
    text = file.read_text()
    return parse_body(file, path, text)


def parse_body(file: Path, path: tuple[str, ...], text: str) -> Module:
    children: list[Module] = []
    masked_ranges: list[tuple[int, int]] = []
    clean = strip_comments_and_strings(text)
    for d in module_decls(clean):
        attrs = attributes_before(text, d.start)
        if is_test_only(attrs):
            continue
        child_path = path + (d.name,)
        if d.body is not None:
            masked_ranges.append(d.body)
            body = text[d.body[0]:d.body[1]]
            children.append(parse_body(file, child_path, body))
        else:
            attr = path_attr(attrs)
            if attr is not None:
                child_file = (file.parent / attr).resolve()
            else:
                candidates = [
                    file.parent / f"{d.name}.rs",
                    file.parent / d.name / "mod.rs",
                ]
                found = [p for p in candidates if p.is_file()]
                if not found:
                    raise SystemExit(
                        f"{file}: cannot locate source for `mod {d.name};` "
                        f"(tried {', '.join(map(str, candidates))})"
                    )
                if len(found) > 1:
                    raise SystemExit(
                        f"{file}: ambiguous source for `mod {d.name};`: "
                        f"{', '.join(map(str, found))}"
                    )
                child_file = found[0]
            children.append(parse_tree(child_file, child_path))
    return Module(path=path, file=file, scan=mask(clean, masked_ranges), children=children)


def flatten(module: Module, out: list[Module]) -> None:
    for child in module.children:
        flatten(child, out)
    out.append(module)


def is_ancestor(a: tuple[str, ...], b: tuple[str, ...]) -> bool:
    return len(a) < len(b) and b[: len(a)] == a


# ---------------------------------------------------------------------------
# Imports

USE_RE = re.compile(r"\buse\s+([^;]+);")
QUALIFIED_RE = re.compile(r"\b(?:crate|super|self)(?:::[A-Za-z_][A-Za-z0-9_]*)+\b")


def split_use_tree(spec: str) -> list[str]:
    """Expand a Rust use tree into its leaf paths."""
    spec = spec.strip()
    if spec.startswith("::"):
        spec = spec[2:]
    leaves: list[str] = []

    def walk(prefix: str, s: str) -> None:
        s = s.strip()
        if not s:
            return
        parts: list[str] = []
        depth, last = 0, 0
        for k, ch in enumerate(s):
            if ch in "{(":
                depth += 1
            elif ch in "})":
                depth -= 1
            elif ch == "," and depth == 0:
                parts.append(s[last:k])
                last = k + 1
        parts.append(s[last:])
        for part in parts:
            part = part.strip()
            if not part:
                continue
            if part.startswith("{") and part.endswith("}"):
                walk(prefix, part[1:-1])
                continue
            part = re.split(r"\s+as\s+", part)[0].strip()
            if part.endswith("::*"):
                leaves.append(prefix + part[:-3])
                continue
            brace = part.find("{")
            if brace >= 0 and part.endswith("}"):
                base = part[:brace]
                walk(prefix + base + ("" if base.endswith("::") else "::"),
                     part[brace + 1:-1])
            else:
                leaves.append(prefix + part)

    walk("", spec)
    return leaves


def resolve(
    path: str, current: tuple[str, ...], modules: set[tuple[str, ...]]
) -> tuple[str, ...] | None:
    segs = [s for s in path.split("::") if s]
    if not segs:
        return None
    if segs[0] == "crate":
        bases = [[]]
        rest = segs[1:]
    elif segs[0] == "super":
        base = list(current)
        k = 0
        while k < len(segs) and segs[k] == "super":
            if base:
                base.pop()
            k += 1
        bases = [base]
        rest = segs[k:]
    elif segs[0] == "self":
        bases = [list(current)]
        rest = segs[1:]
    else:
        # A relative path is resolved in the current module first, then at the
        # crate root (Rust 2018 `use` paths still see the current module).
        bases = [list(current), []]
        rest = segs
    for base in bases:
        full = base + rest
        for length in range(len(full), -1, -1):
            candidate = tuple(full[:length])
            if candidate in modules:
                return candidate
    return None


def imports_of(module: Module, modules: set[tuple[str, ...]]) -> set[tuple[str, ...]]:
    found: set[tuple[str, ...]] = set()
    for m in QUALIFIED_RE.finditer(module.scan):
        found.add(resolve(m.group(0), module.path, modules))
    for m in USE_RE.finditer(module.scan):
        for leaf in split_use_tree(m.group(1)):
            found.add(resolve(leaf, module.path, modules))
    found.discard(None)
    found.discard(module.path)
    return found


# ---------------------------------------------------------------------------
# Graph

def strongly_connected(
    graph: dict[tuple[str, ...], set[tuple[str, ...]]],
) -> list[list[tuple[str, ...]]]:
    index: dict[tuple[str, ...], int] = {}
    low: dict[tuple[str, ...], int] = {}
    stack: list[tuple[str, ...]] = []
    on_stack: set[tuple[str, ...]] = set()
    components: list[list[tuple[str, ...]]] = []
    counter = 0

    def visit(v: tuple[str, ...]) -> None:
        nonlocal counter
        index[v] = low[v] = counter
        counter += 1
        stack.append(v)
        on_stack.add(v)
        for w in graph.get(v, ()):
            if w not in index:
                visit(w)
                low[v] = min(low[v], low[w])
            elif w in on_stack:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp: list[tuple[str, ...]] = []
            while True:
                w = stack.pop()
                on_stack.remove(w)
                comp.append(w)
                if w == v:
                    break
            components.append(comp)

    for v in graph:
        if v not in index:
            visit(v)
    return components


def fmt(path: tuple[str, ...]) -> str:
    return "::".join(path) if path else "<crate>"


@dataclass
class Analysis:
    root: Path
    order: list[Module]
    rank: dict[tuple[str, ...], int]
    edges: dict[tuple[str, ...], set[tuple[str, ...]]]


def analyse(root_file: Path) -> Analysis:
    root = parse_tree(root_file)
    order: list[Module] = []
    flatten(root, order)
    rank = {m.path: i for i, m in enumerate(order)}
    modules = set(rank)
    edges: dict[tuple[str, ...], set[tuple[str, ...]]] = {m.path: set() for m in order}
    for m in order:
        for target in imports_of(m, modules):
            if target in modules and not is_ancestor(m.path, target) and not is_ancestor(
                target, m.path
            ):
                edges[m.path].add(target)
    return Analysis(root_file, order, rank, edges)


def check(analysis: Analysis) -> list[str]:
    rank, edges = analysis.rank, analysis.edges
    backwards = [
        (src, dst)
        for src in sorted(edges, key=lambda p: rank[p])
        for dst in sorted(edges[src], key=lambda p: rank[p])
        if rank[dst] >= rank[src]
    ]
    cycles = [
        c for c in strongly_connected(edges) if len(c) > 1 or c[0] in edges.get(c[0], ())
    ]
    if not backwards and not cycles:
        return []

    lines = [f"{analysis.root}:"]
    if cycles:
        lines.append("  import cycles:")
        for comp in cycles:
            members = sorted(comp, key=lambda p: rank[p])
            lines.append("    " + " <-> ".join(fmt(p) for p in members))
    if backwards:
        lines.append(f"  {len(backwards)} backward reference(s):")
        for src, dst in backwards:
            lines.append(
                f"    {fmt(src)} (position {rank[src]}) -> "
                f"{fmt(dst)} (position {rank[dst]})"
            )
    lines.append("")
    lines.append(
        "  Modules are read in postorder: children in `mod` declaration order, "
        "then the parent."
    )
    lines.append("  A module may only refer to modules earlier in that order.")
    return lines


def workspace_libs() -> list[Path]:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    roots = {
        Path(target["src_path"]).resolve()
        for pkg in meta["packages"]
        for target in pkg["targets"]
        if "lib" in target["kind"] or "proc-macro" in target["kind"]
    }
    return sorted(roots)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "roots",
        nargs="*",
        help="lib.rs roots to check (default: every workspace library)",
    )
    args = parser.parse_args()
    roots = [Path(p).resolve() for p in args.roots] or workspace_libs()

    failures: list[str] = []
    for root in roots:
        try:
            report = check(analyse(root))
        except SystemExit as e:
            report = [str(e)]
        if report:
            failures += report + [""]
    if failures:
        print("\n".join(failures).rstrip(), file=sys.stderr)
        return 1
    print(f"module order OK ({len(roots)} crates)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
