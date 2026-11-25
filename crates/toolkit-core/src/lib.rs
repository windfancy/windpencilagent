//! 基础执行设施：Tool trait、沙箱执行器与**通用**工具集。
//!
//! 本 crate 属于 windpencilagent 基座，**不含任何业务工具**。业务原子工具
//! （扫描 / 指纹 / 爆破 / PoC 等）由专业智能树的「业务工具群组」
//! 以插件形式注册（契约见 `wpa-api` 的 `GroupPlugin`）。

mod executor;
pub mod sandbox;

pub mod encoding;
pub mod hooks;
#[cfg(windows)]
pub mod restricted;
pub mod registry_ext;
pub mod risk_policy;
pub mod tools;

pub use executor::ToolkitExecutor;
pub use sandbox::{Sandbox, SandboxConfig, SandboxError};
pub use encoding::{decode_best, decode_output};

pub use crate::registry_ext::ToolRegistryExt;

/// 通用工具集合（无专业语义，任何领域的基座都可直接注册）。
pub mod builtin {
    pub use crate::tools::ext::{ExtTool, ExtToolSpec};
    pub use crate::tools::file::{FileReadTool, FileWriteTool};
    pub use crate::tools::procexec::ProcExec;
    pub use crate::tools::pysandbox::{PipTool, PythonSandboxComponent, PySandboxTool};
    pub use crate::tools::session_search::SessionSearchTool;
    pub use crate::tools::test::Echo;
}
