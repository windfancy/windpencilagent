#!/usr/bin/env python3
"""编译全部原子插件，并把 dll 放到各自的插件目录里。

插件目录的最终形态（宿主只认这个形状）：

    plugins/vuln_verify/response_diff/
        PnC.toml            元信息（不 dlopen 也能读）
        response_diff.dll   实现

用法：
    python tools/build_plugins.py             # 全量
    python tools/build_plugins.py response_diff text_extract   # 只编指定插件
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PLUGINS = os.path.join(ROOT, "plugins")
OUT = os.path.join(PLUGINS, "target", "release")


def discover() -> list[tuple[str, str]]:
    out = []
    for group in sorted(os.listdir(PLUGINS)):
        gdir = os.path.join(PLUGINS, group)
        if not os.path.isdir(gdir) or group.startswith((".", "_")) or group == "target":
            continue
        for plugin in sorted(os.listdir(gdir)):
            pdir = os.path.join(gdir, plugin)
            if os.path.isfile(os.path.join(pdir, "Cargo.toml")):
                out.append((group, plugin))
    return out


def main() -> int:
    only = set(sys.argv[1:])
    subprocess.run([sys.executable, os.path.join(ROOT, "tools", "sync_plugins_workspace.py")], check=True)

    cmd = ["cargo", "build", "--release"]
    for group, plugin in discover():
        if only and plugin not in only and group not in only:
            continue
        cmd += ["-p", plugin]
    if len(cmd) == 3:
        print("没有匹配的插件")
        return 1

    print("构建：", " ".join(cmd))
    r = subprocess.run(cmd, cwd=PLUGINS)
    if r.returncode != 0:
        print("构建失败")
        return r.returncode

    copied = []
    for group, plugin in discover():
        if only and plugin not in only and group not in only:
            continue
        src = os.path.join(OUT, f"{plugin}.dll")
        dst_dir = os.path.join(PLUGINS, group, plugin)
        dst = os.path.join(dst_dir, f"{plugin}.dll")
        if not os.path.isfile(src):
            print(f"  跳过 {plugin}：找不到 {src}")
            continue
        shutil.copy2(src, dst)
        size = os.path.getsize(dst) / 1024
        copied.append((plugin, size))

    total = sum(s for _, s in copied)
    print(f"\n已放置 {len(copied)} 个插件，合计 {total/1024:.1f} MB")
    for name, size in copied:
        print(f"  {name:32s} {size:7.0f} KB")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
