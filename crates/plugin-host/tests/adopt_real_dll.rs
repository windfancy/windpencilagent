//! 端到端：把真实的插件 dll 丢到 `plugins/` 根目录，`bootstrap` 应该把它收编成
//! `unassigned/<id>/`（dll + 自动生成的 PnC.toml），随后的 `scan` 能看见它。
//!
//! 这条链路依赖三件事同时成立，任一处退化都会让"拷进来就能用"失效：
//! 1. `pnc-probe` 能被找到并跑起来（子进程、超时）；
//! 2. dll 内嵌清单能解析成 `DllManifest`；
//! 3. 落盘后的 `PnC.toml` 能被 `load_plugin` 读回来。

use std::path::{Path, PathBuf};

fn repo_plugin_dll() -> Option<PathBuf> {
    // CARGO_MANIFEST_DIR = <repo>/crates/plugin-host
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let cands = [
        "plugins/file/file.path_info/file_path_info.dll",
        "plugins/file/file.demo/file_demo.dll",
    ];
    cands
        .iter()
        .map(|c| repo.join(c))
        .find(|p| p.is_file())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("wbp_e2e_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn loose_dll_is_adopted_into_unassigned() {
    let Some(src) = repo_plugin_dll() else {
        // 仓库里没有编译好的示例插件（CI 只跑 lib 测试时正常）：不静默跳过，
        // 而是打出原因，避免"测试绿了但其实没测"。
        eprintln!("跳过：仓库内没有已编译的示例插件 dll（{}）", env!("CARGO_MANIFEST_DIR"));
        return;
    };

    let root = tmp("loose");
    let loose = root.join(src.file_name().unwrap());
    std::fs::copy(&src, &loose).unwrap();

    let rep = plugin_host::bootstrap(&root, true);
    assert!(
        rep.problems.is_empty(),
        "收编不该有错误：{:?}",
        rep.problems
    );
    assert_eq!(rep.adopted.len(), 1, "应恰好收编一个");
    let id = rep.adopted[0].clone();

    // 原位置的裸 dll 必须搬走，否则下次启动会重复收编
    assert!(!loose.exists(), "裸 dll 应已移走");

    let dir = root.join("unassigned").join(&id);
    assert!(dir.join("PnC.toml").is_file(), "应生成清单");
    assert!(
        dir.join(src.file_name().unwrap()).is_file(),
        "dll 应搬到插件子目录里"
    );

    // 扫描能看见：它必须是 unassigned 组的成员
    let r = plugin_host::scan::scan(&root);
    let g = r.group("unassigned").expect("未分配组应可扫描");
    assert!(
        g.members.iter().any(|m| m.id == id),
        "收编后应成为未分配组成员，实际：{:?}",
        g.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>()
    );

    // 清单里的 id 必须与目录一致，否则 Loader::ensure 会拒绝加载
    let m = plugin_host::manifest::load_plugin(&dir, "unassigned").unwrap();
    assert_eq!(m.id, id);
    assert_eq!(m.group, "unassigned");
    assert!(m.has_binary(), "dll 就在旁边，不该报缺文件");

    // 再跑一次：幂等，不该重复收编，也不该报错
    let again = plugin_host::bootstrap(&root, true);
    assert!(again.adopted.is_empty(), "第二次启动不应再收编");
    assert!(again.problems.is_empty(), "{:?}", again.problems);
}

#[test]
fn bootstrap_without_auto_probe_leaves_files_alone() {
    let root = tmp("noauto");
    // 造一个"看起来像插件"的裸文件；关掉自举后它必须原地不动
    let stray = root.join("stray.dll");
    std::fs::write(&stray, b"not a real dll").unwrap();

    let rep = plugin_host::bootstrap(&root, false);
    assert!(rep.adopted.is_empty());
    assert!(stray.is_file(), "关掉自举后不该动用户文件");
    assert!(
        root.join("unassigned").join("PnC.toml").is_file(),
        "但未分配目录仍要就位（移出组要靠它）"
    );
}
