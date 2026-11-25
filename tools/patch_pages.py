"""给 app.slint 视图区的 11 个页面补显式几何尺寸。

Slint 铁律（本项目实测）：普通 Rectangle 容器里的子元素默认定位诡异
（会渲染在中部）、且 visible:false 的兄弟仍占位。这里统一给每个页面
显式 x/y/width/height，并绑定到视图区 id，切换页面才真正占满。
"""

import re

PATH = "crates/windbot-ui/ui/app.slint"

src = open(PATH, encoding="utf-8").read()

old_head = """                // 视图区
                Rectangle {
                    vertical-stretch: 1;"""
new_head = """                // 视图区
                view-area := Rectangle {
                    vertical-stretch: 1;"""
assert old_head in src, "视图区头部未找到"
src = src.replace(old_head, new_head, 1)

pat = re.compile(r"(?m)^(\s*)visible: root\.page == (\d+);\n")


def add_geom(m):
    indent = m.group(1)
    return (
        m.group(0)
        + f"{indent}x: 0;\n"
        + f"{indent}y: 0;\n"
        + f"{indent}width: view-area.width;\n"
        + f"{indent}height: view-area.height;\n"
    )


src, n = pat.subn(add_geom, src)
print("patched pages:", n)

open(PATH, "w", encoding="utf-8").write(src)
