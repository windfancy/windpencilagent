//! R5-1（dsh D11 后台任务）：长命令转后台执行——job 输出环（保尾）+ 增量读取 + kill。
//!
//! 语义对齐 dsh run_in_background/job_list/job_output/job_kill：
//! - `shell.run` 带 `background: true` → 落入 JobStore 返回 job_id（立即返回，不阻塞）；
//! - `shell.jobs` 工具三动作：list / output（增量，记 read 偏移）/ kill。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use toolkit_core::sandbox::{is_credential_var, scrubbed_env};

/// 单 job 输出上限（1MB，防失控扫描撑爆内存；超限标记 truncated 停写）。
const OUTPUT_CAP: usize = 1024 * 1024;

pub struct Job {
    pub id: u64,
    pub command: String,
    /// 聚合输出（stdout+stderr 混流，保尾）
    pub output: Mutex<Vec<u8>>,
    /// 是否已结束
    pub done: AtomicBool,
    /// 退出码（-1=被 kill/未结束）
    pub exit_code: AtomicI64,
    /// truncated 标记（输出超限）
    pub truncated: AtomicBool,
    /// 增量读取偏移（job_output 每次读取后推进）
    pub read_pos: Mutex<usize>,
    started_ms: u64,
    /// 子进程句柄（tokio Mutex：wait().await 需跨 await 持锁，std guard 非 Send）
    child: tokio::sync::Mutex<Option<tokio::process::Child>>,
}

impl Job {
    /// 输出追加（保尾：超限截头并标记 truncated）。
    fn append_output(&self, bytes: &[u8]) {
        let mut out = self.output.lock().unwrap();
        if self.truncated.load(Ordering::Relaxed) {
            return;
        }
        out.extend_from_slice(bytes);
        if out.len() > OUTPUT_CAP {
            let drop = out.len() - OUTPUT_CAP;
            // 截头（丢弃前 drop 字节）
            out.drain(..drop);
            self.truncated.store(true, Ordering::Relaxed);
        }
    }

    fn output_str(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }
}

/// 后台任务库（进程级共享）。
pub struct JobStore {
    next: AtomicU64,
    jobs: Mutex<HashMap<u64, Arc<Job>>>,
}

impl JobStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { next: AtomicU64::new(1), jobs: Mutex::new(HashMap::new()) })
    }

    /// 启动后台任务：spawn 子进程 + 聚合任务收集输出；立即返回 job_id。
    pub fn spawn(&self, state: &crate::ShellState, command: &str) -> Result<u64, String> {
        let (program, args) = state.kind.spawn_args(command);
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&args)
            .current_dir(&state.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // R4-5 凭据洗涤（与前台 run_command 同规则）
        cmd.env_clear();
        for (k, v) in scrubbed_env() {
            cmd.env(k, v);
        }
        for (k, v) in &state.env {
            if !is_credential_var(k) {
                cmd.env(k, v);
            }
        }
        let child = cmd.spawn().map_err(|e| format!("后台进程启动失败: {e}"))?;

        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let job = Arc::new(Job {
            id,
            command: command.to_string(),
            output: Mutex::new(Vec::new()),
            done: AtomicBool::new(false),
            exit_code: AtomicI64::new(-1),
            truncated: AtomicBool::new(false),
            read_pos: Mutex::new(0),
            started_ms: now_ms(),
            child: tokio::sync::Mutex::new(Some(child)),
        });

        // stdout/stderr 各自聚合成流：take 出 reader，逐 chunk append（保尾）
        let job_out = job.clone();
        tokio::spawn(async move {
            let (stdout, stderr) = {
                let mut guard = job_out.child.lock().await;
                match guard.as_mut() {
                    Some(c) => (c.stdout.take(), c.stderr.take()),
                    None => (None, None),
                }
            };
            let mut tasks = Vec::new();
            if let Some(r) = stdout {
                let j = job_out.clone();
                tasks.push(tokio::spawn(async move { pump(r, j).await }));
            }
            if let Some(r) = stderr {
                let j = job_out.clone();
                tasks.push(tokio::spawn(async move { pump(r, j).await }));
            }
            // wait 子进程记录退出码（tokio Mutex guard 可跨 await）
            let code = {
                let mut guard = job_out.child.lock().await;
                match guard.as_mut() {
                    Some(c) => c.wait().await.ok().and_then(|s| s.code()).map(|c| c as i64),
                    None => None,
                }
            };
            job_out.exit_code.store(code.unwrap_or(-1), Ordering::Relaxed);
            job_out.done.store(true, Ordering::Relaxed);
            for t in tasks {
                let _ = t.await;
            }
        });

        self.jobs.lock().unwrap().insert(id, job);
        Ok(id)
    }

    /// 任务概要列表：(id, command, done, exit_code, started_ms)。
    pub fn list(&self) -> Vec<(u64, String, bool, i64, u64)> {
        self.jobs
            .lock()
            .unwrap()
            .values()
            .map(|j| (j.id, j.command.clone(), j.done.load(Ordering::Relaxed), j.exit_code.load(Ordering::Relaxed), j.started_ms))
            .collect()
    }

    /// 增量读取输出：返回 (新的读取偏移, 新增文本, 是否已结束, 退出码)。读后推进偏移。
    pub fn output(&self, id: u64) -> Option<(usize, String, bool, i64)> {
        let job = self.jobs.lock().unwrap().get(&id).cloned()?;
        let raw = job.output_str();
        let mut pos = job.read_pos.lock().unwrap();
        let new_part: String = raw.chars().skip(*pos).collect();
        *pos += new_part.chars().count();
        Some((
            *pos,
            new_part,
            job.done.load(Ordering::Relaxed),
            job.exit_code.load(Ordering::Relaxed),
        ))
    }

    /// 强制终止任务；返回是否找到。
    pub async fn kill(&self, id: u64) -> bool {
        let job = self.jobs.lock().unwrap().get(&id).cloned();
        match job {
            Some(j) => {
                if let Some(c) = j.child.lock().await.as_mut() {
                    let _ = c.start_kill();
                }
                true
            }
            None => false,
        }
    }

    /// 清理已结束任务（释放 child 句柄），返回清理数。
    pub fn reap(&self) -> usize {
        let mut map = self.jobs.lock().unwrap();
        let before = map.len();
        map.retain(|_, j| !j.done.load(Ordering::Relaxed));
        before - map.len()
    }
}

async fn pump(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    job: Arc<Job>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => job.append_output(&buf[..n]),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
