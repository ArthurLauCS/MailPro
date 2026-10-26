"""重新生成 THIRD_PARTY_NOTICES.txt：安装包里打包的第三方代码的许可证。

Rust 依赖取自 cargo metadata（Windows x64 目标，不含只在测试里用的依赖），许可证原文取自各 crate 目录里的
LICENSE / COPYING / NOTICE 文件。相同的许可证原文只收一份，各条目按编号引用。依赖变了之后运行：python scripts/notices.py
"""
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NAMES = ("license", "licence", "copying", "notice", "unlicense")


def license_files(directory: Path):
    return sorted(p for p in directory.iterdir() if p.is_file() and p.name.lower().startswith(NAMES))


def rust_packages():
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc", "--locked"],
        cwd=ROOT, check=True, capture_output=True, text=True, encoding="utf-8").stdout)
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    packages = {p["id"]: p for p in meta["packages"]}
    members = set(meta["workspace_members"])
    seen, stack = set(), list(members)
    while stack:
        pid = stack.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            # 只在测试里用的依赖不进安装包
            if any(k["kind"] != "dev" for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    out = []
    for pid in seen - members:
        p = packages[pid]
        directory = Path(p["manifest_path"]).parent
        out.append((p["name"], p["version"], p.get("repository") or "", p.get("license") or "", license_files(directory)))
    return sorted(out, key=lambda x: (x[0], x[1]))


def main():
    entries = rust_packages()
    assets = ROOT / "crates" / "app" / "assets"
    entries.append(("Noto Serif SC (font) 2.003", "", "https://github.com/notofonts/noto-cjk", "OFL-1.1", [assets / "OFL-NotoSerifSC.txt"]))
    entries.append(("LXGW WenKai (font) 1.530", "", "https://github.com/lxgw/LxgwWenKai", "OFL-1.1", [assets / "OFL-LXGWWenKai.txt"]))
    texts: dict[str, int] = {}
    blocks = []
    for name, version, repo, license, files in entries:
        lines = [f"{name} {version}"]
        if repo:
            lines.append(repo)
        lines.append(f"License: {license}")
        for f in files:
            text = f.read_text(encoding="utf-8", errors="replace").strip()
            n = texts.setdefault(text, len(texts) + 1)
            lines.append(f"{f.name}: License text {n}")
        blocks.append("\n".join(lines))
    head = ("MailPro - Third-party notices\n\n"
            "These notices cover the Rust dependencies built into MailPro (including build dependencies) and bundled fonts. "
            "They do not license MailPro itself. Identical license texts are included once and referenced below.")
    bar = "=" * 72
    bodies = [f"{bar}\nLicense text {n}\n{bar}\n\n{text}" for text, n in texts.items()]
    (ROOT / "THIRD_PARTY_NOTICES.txt").write_text("\n\n".join([head, *blocks, *bodies]) + "\n", encoding="utf-8", newline="\n")
    print(f"{len(entries)} packages, {len(texts)} license texts")


if __name__ == "__main__":
    main()
