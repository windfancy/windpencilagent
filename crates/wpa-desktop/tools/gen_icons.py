"""生成 ui/kit/icons/ 下的占位图标。

kit 的 icons.slint 引用了 26 个 svg，但源项目未随附图片资源。
这里按名字生成等价的 24x24 线性图标（stroke 风格，无填充），
由 Slint 内置的 resvg 渲染。仅用于占位，不影响功能。
"""

import os

OUT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "ui", "kit", "icons")

# 每个图标一组 <path> 的 d 属性
S = "stroke='#000000' stroke-width='2' stroke-linecap='round' stroke-linejoin='round' fill='none'"

ICONS = {
    "add": ["M12 5v14", "M5 12h14"],
    "remove": ["M5 12h14"],
    "check": ["M4 12.5l5 5L20 6.5"],
    "close": ["M6 6l12 12", "M18 6L6 18"],
    "backspace": ["M9 5h11v14H9L3 12z", "M13 9l5 6", "M18 9l-5 6"],
    "arrow_upward": ["M12 20V5", "M5 11l7-7 7 7"],
    "arrow_downward": ["M12 4v15", "M5 13l7 7 7-7"],
    "thermostat": ["M12 3v10", "M12 21a4 4 0 0 0 2-7.5V5a2 2 0 0 0-4 0v8.5A4 4 0 0 0 12 21z"],
    "co2": ["M4 14a4 4 0 1 0 4-4", "M14 10h4l-4 8h4"],
    "water_drop": ["M12 3c0 0-6 7-6 11a6 6 0 0 0 12 0c0-4-6-11-6-11z"],
    "local_fire_department": ["M12 3c0 0-4 5-4 9a4 4 0 0 0 8 0c0-2-1-3-1-3s-1 2-2 2c-1-4-1-8-1-8z"],
    "schedule": ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 7v5l3 2"],
    "settings": ["M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7z",
                 "M12 2v3", "M12 19v3", "M2 12h3", "M19 12h3",
                 "M5 5l2 2", "M17 17l2 2", "M19 5l-2 2", "M7 17l-2 2"],
    "factory": ["M3 20h18V9l-6 4V9l-6 4V9H3z", "M7 20v-3", "M12 20v-3", "M17 20v-3"],
    "history": ["M3 12a9 9 0 1 0 3-6.7", "M3 4v5h5", "M12 8v4l3 2"],
    "dashboard": ["M4 4h7v7H4z", "M13 4h7v5h-7z", "M13 11h7v9h-7z", "M4 13h7v7H4z"],
    "home": ["M3 11l9-7 9 7", "M6 10v10h12V10"],
    "menu": ["M4 7h16", "M4 12h16", "M4 17h16"],
    "list": ["M4 7h16", "M4 12h16", "M4 17h16", "M2 7h.01", "M2 12h.01", "M2 17h.01"],
    "grid_view": ["M4 4h6v6H4z", "M14 4h6v6h-6z", "M4 14h6v6H4z", "M14 14h6v6h-6z"],
    "warning": ["M12 3l9 17H3z", "M12 9v5", "M12 17h.01"],
    "error": ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 8v5", "M12 16h.01"],
    "info": ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 11v5", "M12 8h.01"],
    "notifications": ["M6 17V11a6 6 0 0 1 12 0v6", "M4 17h16", "M10 20a2 2 0 0 0 4 0"],
    "lock": ["M5 11h14v10H5z", "M8 11V8a4 4 0 0 1 8 0v3"],
    "mode_fan": ["M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18z", "M12 12c0-3 1-6 4-6",
                 "M12 12c3 0 6 1 6 4", "M12 12c-3 0-6-1-6-4"],
}

TPL = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" width="24" height="24">\n'
       '{body}</svg>\n')


def main():
    os.makedirs(OUT_DIR, exist_ok=True)
    for name, paths in ICONS.items():
        body = "".join(f"  <path d='{d}' {S}/>\n" for d in paths)
        with open(os.path.join(OUT_DIR, name + ".svg"), "w", encoding="utf-8") as f:
            f.write(TPL.format(body=body))
    print(f"wrote {len(ICONS)} icons -> {os.path.normpath(OUT_DIR)}")


if __name__ == "__main__":
    main()
