#!/usr/bin/env python3
"""同步 plugins/ 的 workspace 成员列表。

Cargo 的 glob 遇到"没有子 crate 的组目录"会直接报错，所以这里改用显式列表：
扫描 `plugins/<group>/<plugin>/Cargo.toml`，逐个写进成员表。新增插件后跑一次即可。

用法：
    python tools/sync_plugins_workspace.py [--check]

`--check` 只检查是否需要同步（CI/提交前用），不一致时退出码 1。
"""

from __future__ import annotations

import io
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PLUGINS = os.path.join(ROOT, "plugins")
CARGO = os.path.join(PLUGINS, "Cargo.toml")

HEADER = """[workspace]
resolver = "2"
members = [
    "plugin-sdk",
"""

FOOTER = """]

[workspace.package]
version = "0.1.0"
edition = "2021"

[workspace.dependencies]
serde_json = "1"
plugin-sdk = { path = "plugin-sdk" }

# 插件是 cdylib：lto + strip 把 75 个插件的总体积压到 ~10MB。
# panic 必须是 unwind —— SDK 靠 catch_unwind 兜住插件内部 panic，
# 改成 abort 会让任何一个插件 panic 直接带走整个进程。
[profile.release]
opt-level = "z"
lto = true
strip = true
codegen-units = 1
panic = "unwind"
"""


def discover() -> list[str]:
    out: list[str] = []
    if not os.path.isdir(PLUGINS):
        return out
    for group in sorted(os.listdir(PLUGINS)):
        gdir = os.path.join(PLUGINS, group)
        if not os.path.isdir(gdir) or group.startswith((".", "_")) or group == "target":
            continue
        for plugin in sorted(os.listdir(gdir)):
            pdir = os.path.join(gdir, plugin)
            if os.path.isfile(os.path.join(pdir, "Cargo.toml")):
                out.append(f'    "{group}/{plugin}",')
    return out


def render(members: list[str]) -> str:
    return HEADER + "".join(m + "\n" for m in members) + FOOTER


def main() -> int:
    check = "--check" in sys.argv
    members = discover()
    text = render(members)
    current = ""
    if os.path.isfile(CARGO):
        current = io.open(CARGO, encoding="utf-8").read()
    if current == text:
        print(f"workspace 已同步（{len(members)} 个插件）")
        return 0
    if check:
        print("workspace 未同步：请运行 python tools/sync_plugins_workspace.py")
        return 1
    io.open(CARGO, "w", encoding="utf-8", newline="\n").write(text)
    print(f"已写入 {len(members)} 个插件成员")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
