"""把页面组件的根布局包进一个铺满的 Rectangle。

问题：`component XxxPage { VerticalLayout { ... } }` 这种根是布局的组件，
在实例上写 `width: view-area.width` 会被布局的首选尺寸覆盖 —— 实测记忆页
只拿到 455px 宽、内容垂直居中（Slint 铁律：布局根组件不吃外部几何约束）。

做法：给这些组件的根布局外面垫一层 Rectangle，由它吃 parent 的尺寸，
布局本身再用显式 x/y/width/height 填满这层 Rectangle。
"""

import re
import sys

PATH = "crates/windbot-ui/ui/app.slint"

# 需要包裹的组件（根是布局，且在视图区/固定栏里要铺满）
TARGETS = [
    "ChatPage",
    "TaskPage",
    "TreePage",
    "ToolsPage",
    "EmptySlotPage",
    "SpacePage",
    "MemoryPage",
    "ModelPage",
    "SettingsPage",
    "KitGallery",
    "RightRail",
]

LAYOUTS = ("VerticalLayout", "HorizontalLayout")


def find_block(lines, start, brace_line):
    """从含 '{' 的行开始，找到与之配对的 '}' 行号。"""
    depth = 0
    for i in range(start, len(lines)):
        depth += lines[i].count("{") - lines[i].count("}")
        if depth == 0 and i >= brace_line:
            return i
    raise RuntimeError(f"未找到配对花括号：行 {start}")


def wrap_component(lines, name):
    # 1) 定位组件声明
    head = None
    for i, ln in enumerate(lines):
        if re.match(rf"^component {name} \{{$", ln.strip()) and ln.startswith("component "):
            head = i
            break
    if head is None:
        print(f"  ! 未找到组件 {name}")
        return lines, False
    body_end = find_block(lines, head, head)
    # 2) 找根布局（4 空格缩进的第一行布局）
    root = None
    for i in range(head + 1, body_end):
        s = lines[i]
        if any(s.startswith("    " + lay + " {") for lay in LAYOUTS):
            root = i
            break
    if root is None:
        print(f"  - {name}: 根不是布局，跳过")
        return lines, False
    root_end = find_block(lines, root, root)
    # 3) 重新缩进布局体
    inner = ["    " + ln if ln.strip() else ln for ln in lines[root : root_end + 1]]
    indent = "    "
    wrapper = [
        f"{indent}Rectangle {{\n",
        f"{indent}    x: 0;\n",
        f"{indent}    y: 0;\n",
        f"{indent}    width: parent.width;\n",
        f"{indent}    height: parent.height;\n",
        "\n",
        f"{indent}    {lines[root].strip()}\n",
        f"{indent}        x: 0;\n",
        f"{indent}        y: 0;\n",
        f"{indent}        width: parent.width;\n",
        f"{indent}        height: parent.height;\n",
    ]
    # inner[0] 就是布局声明行，已由 wrapper 末尾替代，从 inner[1:] 开始接上
    new_body = inner[1:-1]  # 去掉首行声明与末行 '}'
    tail = [f"{indent}    }}\n", f"{indent}}}\n"]
    lines = lines[:root] + wrapper + new_body + tail + lines[root_end + 1 :]
    print(f"  + {name}: 已包裹（布局行 {root + 1}）")
    return lines, True


def main():
    src = open(PATH, encoding="utf-8").read()
    lines = src.splitlines(keepends=True)
    changed = 0
    for name in TARGETS:
        lines, ok = wrap_component(lines, name)
        changed += 1 if ok else 0
    open(PATH, "w", encoding="utf-8").write("".join(lines))
    print(f"done: {changed} 个组件已包裹")
    return 0


if __name__ == "__main__":
    sys.exit(main())
