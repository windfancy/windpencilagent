//! 沙箱执行器：子进程 + 资源限制 + 输出配额 + 审计元数据。

use std::time::Instant;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::Duration;

use kern_core::model::ExecutionContext;

use crate::encoding::decode_output;

/// R4-5（dsh X6 凭据洗涤）：环境变量名是否疑似凭据（KEY/PASSWORD/SECRET/TOKEN/
/// CREDENTIAL 形命名，大小写不敏感）。
pub fn is_credential_var(name: &str) -> bool {
    let ku = name.to_ascii_uppercase();
    ku.contains("KEY")
        || ku.contains("PASSWORD")
        || ku.contains("PASSWD")
        || ku.contains("SECRET")
        || ku.contains("TOKEN")
        || ku.contains("CREDENTIAL")
}

/// 洗涤后的父进程环境快照：剥离疑似凭据变量。
/// 用法：`cmd.env_clear(); for (k,v) in scrubbed_env() { cmd.env(k,v); }`
pub fn scrubbed_env() -> Vec<(String, String)> {
    std::env::vars().filter(|(k, _)| !is_credential_var(k)).collect()
}

/// 沙箱错误。
#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("command spawn failed: {0}")]
    Spawn(std::io::Error),
    #[error("command timed out after {max_ms}ms")]
    Timeout { max_ms: u64 },
    #[error("command failed with exit code {code}: {stderr}")]
    NonZeroExit { code: i32, stderr: String },
}

/// 一次沙箱执行的结果（可落审计）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxOutput {
    pub ok: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// 沙箱配置。
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    pub max_exec_ms: u64,
    pub max_output_bytes: usize,
    pub max_retries: u32,
    /// R5-6：受限令牌执行（Safer NORMALUSER，fail-closed）；默认关闭（opt-in）
    pub restricted: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            max_exec_ms: 30_000,
            max_output_bytes: 512 * 1024,
            max_retries: 2,
            restricted: false,
        }
    }
}

/// 进程沙箱。所有外部命令统一经由它执行，保证限时、限配额、可审计。
#[derive(Clone)]
pub struct Sandbox {
    cfg: SandboxConfig,
}

impl Sandbox {
    pub fn new(cfg: SandboxConfig) -> Self {
        Self { cfg }
    }

    pub fn config(&self) -> &SandboxConfig {
        &self.cfg
    }

    /// 执行一条命令并返回（截断的）输出。
    ///
    /// `ctx` 用于审计；授权校验由 ScopeGuard 统一承担，这里专注限时与配额。
    #[allow(clippy::too_many_arguments)]
    pub async fn exec(
        &self,
        ctx: &ExecutionContext,
        program: &str,
        args: &[&str],
    ) -> Result<SandboxOutput, SandboxError> {
        let start = Instant::now();

        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // R4-5 凭据洗涤：子进程环境剥离 KEY/PASSWORD/SECRET/TOKEN 形命名
        cmd.env_clear();
        for (k, v) in scrubbed_env() {
            cmd.env(k, v);
        }

        // R5-6：受限令牌模式（opt-in）——Safer NORMALUSER 令牌 + CreateProcessAsUserW。
        // fail-closed：令牌创建任一步失败即拒绝执行；超时在 FFI 内 WaitForSingleObject
        // 超时 + TerminateProcess 自处理，无需外层取消。
        if self.cfg.restricted {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let spec = crate::restricted::RestrictedSpec {
                program: program.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd,
                env: Vec::new(), // 环境块由 CreateEnvironmentBlock 从受限令牌构建
                timeout_ms: self.cfg.max_exec_ms.min(u32::MAX as u64) as u32,
            };
            crate::restricted::validate_cwd(&spec.cwd)
                .map_err(|e| SandboxError::Spawn(std::io::Error::other(e)))?;
            let out = tokio::task::spawn_blocking(move || crate::restricted::launch_restricted(&spec))
                .await
                .map_err(|e| SandboxError::Spawn(std::io::Error::other(e.to_string())))?
                .map_err(|e| SandboxError::Spawn(std::io::Error::other(e)))?;
            if out.timed_out {
                return Err(SandboxError::Timeout { max_ms: self.cfg.max_exec_ms });
            }
            let stdout = decode_output(out.stdout.as_bytes(), self.cfg.max_output_bytes);
            let stderr = decode_output(out.stderr.as_bytes(), self.cfg.max_output_bytes);
            let duration_ms = start.elapsed().as_millis() as u64;
            let ok = out.exit_code == 0;
            return Ok(SandboxOutput {
                ok,
                exit_code: Some(out.exit_code),
                stdout,
                stderr,
                duration_ms,
            });
        }

        let child = cmd.spawn().map_err(SandboxError::Spawn)?;

        // 用 timeout 包裹 wait_with_output；超时后 future 被丢弃，
        // child 会随 drop 触发 kill_on_drop(true) 终止进程。
        let output = match tokio::time::timeout(
            Duration::from_millis(self.cfg.max_exec_ms),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(SandboxError::Spawn(e)),
            Err(_) => {
                return Err(SandboxError::Timeout {
                    max_ms: self.cfg.max_exec_ms,
                });
            }
        };

        let stdout = decode_output(&output.stdout, self.cfg.max_output_bytes);
        let stderr = decode_output(&output.stderr, self.cfg.max_output_bytes);

        let duration_ms = start.elapsed().as_millis() as u64;
        let exit_code = output.status.code();
        let ok = output.status.success();

        tracing::info!(
            session = %ctx.session_id.as_uuid(),
            task = %ctx.task_id.as_ref(),
            program,
            exit_code = tracing::field::debug(exit_code),
            duration_ms,
            "sandbox.exec"
        );

        if !ok {
            return Err(SandboxError::NonZeroExit {
                code: exit_code.unwrap_or(-1),
                stderr,
            });
        }

        Ok(SandboxOutput {
            ok,
            exit_code,
            stdout,
            stderr,
            duration_ms,
        })
    }
}
