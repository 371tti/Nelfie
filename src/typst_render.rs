use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration as StdDuration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Datelike;
use flate2::read::GzDecoder;
use log::warn;
use serde::{Deserialize, Serialize};
use tar::Archive;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    time::timeout,
};
use typst::{
    Library, LibraryExt, World,
    diag::FileError,
    foundations::{Bytes, Datetime, Duration},
    syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot, package::PackageSpec},
    text::{Font, FontBook},
    utils::LazyHash,
};
use typst_layout::PagedDocument;

use crate::{app::context::NelfieContext, discord::attachments::AttachmentRef};

const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
const MAX_TOTAL_INPUT_BYTES: usize = 50 * 1024 * 1024;
const MAX_RENDERED_BYTES: usize = 18 * 1024 * 1024;
const MAX_WORKER_JSON_BYTES: usize = 72 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_ASSETS: usize = 64;
const MAX_PENDING_TYPST_DRAFTS: usize = 8;
const MAX_RENDERED_PAGES: usize = 50;
const MAX_WORKER_OUTPUT_BYTES: usize = MAX_RENDERED_BYTES * 2 + 17 + MAX_RENDERED_PAGES * 8;
const WORKER_DEADLINE: StdDuration = StdDuration::from_secs(120);
const WORKER_OUTPUT_MAGIC: &[u8; 4] = b"NTY2";
const PNG_SCALE: f32 = 2.0;
const MAX_PNG_DIMENSION: f32 = 8192.0;
const MAX_PNG_PIXELS: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_ARCHIVE_BYTES: usize = 25 * 1024 * 1024;
const MAX_PACKAGE_EXPANDED_BYTES: usize = 50 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 4096;
const MAX_PACKAGES_PER_RENDER: usize = 16;

static RENDER_SEMAPHORE: Semaphore = Semaphore::const_new(1);

pub struct TypstAsset {
    pub filename: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

pub struct TypstRenderInput {
    pub source_filename: String,
    pub source: String,
    pub assets: Vec<TypstAsset>,
    pub format: TypstOutputFormat,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TypstOutputFormat {
    #[default]
    Png,
    Svg,
    Pdf,
}

impl TypstOutputFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "png" => Ok(Self::Png),
            "svg" => Ok(Self::Svg),
            "pdf" => Ok(Self::Pdf),
            other => Err(format!("対応形式はpng、svg、pdfです: `{other}`")),
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
            Self::Pdf => "pdf",
        }
    }
}

pub enum TypstRenderContent {
    Pdf(Vec<u8>),
    Pages(Vec<Vec<u8>>),
}

pub struct TypstRenderOutput {
    pub content: TypstRenderContent,
    pub preview_pages: Vec<Vec<u8>>,
    pub warnings: Vec<String>,
}

pub struct TypstDraftStore {
    drafts: Mutex<HashMap<String, PendingTypstDraft>>,
    next_id: AtomicU64,
}

struct PendingTypstDraft {
    format: TypstOutputFormat,
    rendered_round: usize,
    preview_delivered: bool,
    content: TypstRenderContent,
    warnings: Vec<String>,
}

impl TypstDraftStore {
    pub fn ensure_capacity(&self) -> Result<(), String> {
        let drafts = self
            .drafts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if drafts.len() >= MAX_PENDING_TYPST_DRAFTS {
            return Err(format!(
                "この応答ではTypst下書きを最大{MAX_PENDING_TYPST_DRAFTS}件まで保持できます。"
            ));
        }
        Ok(())
    }

    pub fn insert(
        &self,
        format: TypstOutputFormat,
        rendered_round: usize,
        output: TypstRenderOutput,
    ) -> Result<(String, Vec<Vec<u8>>), String> {
        let TypstRenderOutput {
            content,
            preview_pages,
            warnings,
        } = output;
        let mut drafts = self
            .drafts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if drafts.len() >= MAX_PENDING_TYPST_DRAFTS {
            return Err(format!(
                "この応答ではTypst下書きを最大{MAX_PENDING_TYPST_DRAFTS}件まで保持できます。"
            ));
        }
        let id = format!("typst-{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        drafts.insert(id.clone(), PendingTypstDraft {
            format,
            rendered_round,
            preview_delivered: false,
            content,
            warnings,
        });
        Ok((id, preview_pages))
    }

    pub fn mark_preview_delivered(&self, draft_id: &str, round: usize) {
        if let Some(draft) = self
            .drafts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(draft_id)
            && draft.rendered_round == round
        {
            draft.preview_delivered = true;
        }
    }

    pub fn take_reviewed(
        &self,
        draft_id: &str,
        round: usize,
    ) -> Result<(TypstOutputFormat, TypstRenderContent, Vec<String>), String> {
        let mut drafts = self
            .drafts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(draft) = drafts.get(draft_id) else {
            return Err(format!("Typst下書き`{draft_id}`はこの応答にありません。"));
        };
        if !draft.preview_delivered || round <= draft.rendered_round {
            return Err(
                "指定したTypstプレビューを次の応答ラウンドで確認してから公開してください。"
                    .to_string(),
            );
        }
        let draft = drafts
            .remove(draft_id)
            .expect("selected Typst draft was checked");
        Ok((draft.format, draft.content, draft.warnings))
    }
}

impl Default for TypstDraftStore {
    fn default() -> Self {
        Self {
            drafts: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }
}

pub async fn load_registry_asset(
    ob_ctx: &NelfieContext,
    attachment_id: u64,
) -> Result<TypstAsset, String> {
    let reference = ob_ctx
        .attachment_registry
        .find(attachment_id, None)
        .ok_or_else(|| format!("添付ID `{attachment_id}` は保持中の参照にありません。"))?;
    load_asset_reference(ob_ctx, &reference).await
}

pub async fn load_registry_assets(
    ob_ctx: &NelfieContext,
    attachment_ids: &[u64],
    excluded_attachment_id: Option<u64>,
) -> Result<Vec<TypstAsset>, String> {
    let ids = attachment_ids
        .iter()
        .copied()
        .filter(|id| Some(*id) != excluded_attachment_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if ids.len() > MAX_ASSETS {
        return Err("Typst素材は64ファイルまでです。".to_string());
    }
    let references = ob_ctx.attachment_registry.find_many(&ids, None)?;
    if references
        .iter()
        .any(|reference| {
            reference
                .size
                .is_some_and(|size| size > MAX_FILE_BYTES as u64)
        })
    {
        return Err("Typst素材のいずれかが25 MiBを超えています。".to_string());
    }
    if references
        .iter()
        .filter_map(|reference| reference.size)
        .sum::<u64>()
        > MAX_TOTAL_INPUT_BYTES as u64
    {
        return Err("Typst素材の合計は50 MiBまでです。".to_string());
    }
    let mut assets = Vec::with_capacity(references.len());
    let mut loaded_bytes = 0usize;
    for reference in references {
        let asset = load_asset_reference(ob_ctx, &reference).await?;
        loaded_bytes = loaded_bytes.saturating_add(asset.bytes.len());
        if loaded_bytes > MAX_TOTAL_INPUT_BYTES {
            return Err("Typst素材の合計は50 MiBまでです。".to_string());
        }
        assets.push(asset);
    }
    Ok(assets)
}

async fn load_asset_reference(
    ob_ctx: &NelfieContext,
    reference: &AttachmentRef,
) -> Result<TypstAsset, String> {
    let http = ob_ctx
        .discord_client
        .try_open()
        .ok_or_else(|| "Discord接続が利用できません。".to_string())?;
    let (filename, content_type, bytes) = ob_ctx
        .attachment_registry
        .fetch_message_attachment(&http.http, reference)
        .await?;
    Ok(TypstAsset {
        filename,
        content_type,
        bytes,
    })
}

pub fn source_from_asset(asset: TypstAsset) -> Result<(String, String), String> {
    if !asset.filename.to_ascii_lowercase().ends_with(".typ") {
        return Err(format!(
            "メインソースは`.typ`ファイルを選択してください: `{}`",
            asset.filename
        ));
    }
    let source = String::from_utf8(asset.bytes)
        .map_err(|_| format!("`{}` はUTF-8のTypstソースではありません。", asset.filename))?;
    Ok((asset.filename, source))
}

#[derive(Serialize, Deserialize)]
struct WorkerRequest {
    source_filename: String,
    source: String,
    assets: Vec<EncodedAsset>,
    format: TypstOutputFormat,
}

#[derive(Serialize, Deserialize)]
struct EncodedAsset {
    filename: String,
    content_type: Option<String>,
    data: String,
}

pub async fn render(input: TypstRenderInput) -> Result<TypstRenderOutput, String> {
    validate_input(&input)?;
    let format = input.format;
    let permit = RENDER_SEMAPHORE.try_acquire().map_err(|_| {
        "Typstレンダラは現在使用中です。時間をおいて再試行してください。".to_string()
    })?;

    let request = WorkerRequest {
        source_filename: input.source_filename,
        source: STANDARD.encode(input.source.as_bytes()),
        assets: input
            .assets
            .into_iter()
            .map(|asset| EncodedAsset {
                filename: asset.filename,
                content_type: asset.content_type,
                data: STANDARD.encode(asset.bytes),
            })
            .collect(),
        format,
    };
    let payload = serde_json::to_vec(&request)
        .map_err(|error| format!("Typst入力をシリアライズできません: {error}"))?;
    if payload.len() > MAX_WORKER_JSON_BYTES {
        return Err("Typst入力が上限を超えています。".to_string());
    }

    let result = run_isolated_worker(&payload, format).await;
    drop(permit);
    result
}

fn validate_input(input: &TypstRenderInput) -> Result<(), String> {
    if !is_safe_filename(&input.source_filename)
        || !input.source_filename.to_ascii_lowercase().ends_with(".typ")
    {
        return Err("Typstのメインファイル名は安全な`.typ`ファイルにしてください。".to_string());
    }
    let source_bytes = input.source.len();
    if source_bytes > MAX_FILE_BYTES {
        return Err("Typstソースは25 MiBを超えています。".to_string());
    }
    if input.assets.len() > MAX_ASSETS {
        return Err(format!("Typst素材は{MAX_ASSETS}ファイルまでです。"));
    }

    let mut total = source_bytes;
    let mut names = HashSet::from([input.source_filename.to_ascii_lowercase()]);
    for asset in &input.assets {
        if !is_safe_filename(&asset.filename) {
            return Err(format!(
                "安全でない添付ファイル名です: `{}`",
                asset.filename
            ));
        }
        if asset.bytes.len() > MAX_FILE_BYTES {
            return Err(format!("`{}` は25 MiBを超えています。", asset.filename));
        }
        if !names.insert(asset.filename.to_ascii_lowercase()) {
            return Err(format!(
                "同名の素材ファイルがあります: `{}`",
                asset.filename
            ));
        }
        total = total.saturating_add(asset.bytes.len());
    }
    if total > MAX_TOTAL_INPUT_BYTES {
        return Err("Typst入力ファイルの合計は50 MiBまでです。".to_string());
    }
    Ok(())
}

fn is_safe_filename(filename: &str) -> bool {
    !filename.is_empty()
        && filename != "."
        && filename != ".."
        && !filename
            .chars()
            .any(|character| matches!(character, '/' | '\\' | ':' | '\0'))
        && filename.chars().all(|character| !character.is_control())
}

async fn run_isolated_worker(
    payload: &[u8],
    format: TypstOutputFormat,
) -> Result<TypstRenderOutput, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Nelfie実行ファイルを特定できません: {error}"))?;
    let mut command = worker_command(&executable);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| format!("Typstワーカーを起動できません: {error}"))?;
    #[cfg(windows)]
    let job = match windows_job::apply(&child) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(format!("Typstワーカーの制限を設定できません: {error}"));
        }
    };

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Typstワーカーのstdinを取得できません。".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Typstワーカーのstdoutを取得できません。".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Typstワーカーのstderrを取得できません。".to_string())?;
    let diagnostics_task = tokio::spawn(read_worker_diagnostics(stderr));
    let write_input = async move {
        let mut stdin = stdin;
        stdin.write_all(payload).await?;
        // ChildStdin::shutdown is a no-op for process pipes on Windows and Unix.
        // Dropping the handle sends EOF so the worker's read_to_end can finish.
        drop(stdin);
        Ok::<(), std::io::Error>(())
    };

    let execution = timeout(WORKER_DEADLINE, async {
        tokio::try_join!(
            write_input,
            read_limited(stdout, MAX_WORKER_OUTPUT_BYTES),
            child.wait(),
        )
    })
    .await;

    let ((), rendered, status, diagnostics) = match execution {
        Ok(Ok(((), rendered, status))) => {
            let diagnostics = diagnostics_task
                .await
                .map_err(|error| format!("Typst診断を受け取れません: {error}"))?;
            ((), rendered, status, diagnostics)
        }
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            #[cfg(windows)]
            drop(job);
            let diagnostics = diagnostics_task.await.unwrap_or_default();
            log_worker_diagnostics("I/O error", &diagnostics);
            return Err(format!("Typstワーカーとの入出力に失敗しました: {error}"));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            #[cfg(windows)]
            drop(job);
            let diagnostics = diagnostics_task.await.unwrap_or_default();
            log_worker_diagnostics("120-second timeout", &diagnostics);
            return Err("Typstレンダリングが120秒を超えたため終了しました。".to_string());
        }
    };
    #[cfg(windows)]
    drop(job);

    let diagnostics = String::from_utf8_lossy(&diagnostics)
        .lines()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if !status.success() {
        let detail = diagnostics
            .into_iter()
            .filter(|line| !line.is_empty())
            .take(20)
            .collect::<Vec<_>>()
            .join("\n");
        return Err(if detail.is_empty() {
            format!(
                "Typstレンダリングに失敗しました（終了コード {:?}）。",
                status.code()
            )
        } else {
            format!("Typstレンダリングに失敗しました:\n{detail}")
        });
    }
    let (content, preview_pages) = parse_worker_output(rendered, format)?;
    Ok(TypstRenderOutput {
        content,
        preview_pages,
        warnings: diagnostics
            .into_iter()
            .filter(|line| line.starts_with("warning:"))
            .collect(),
    })
}

fn parse_worker_output(
    output: Vec<u8>,
    format: TypstOutputFormat,
) -> Result<(TypstRenderContent, Vec<Vec<u8>>), String> {
    if output.len() < 5 || &output[..4] != WORKER_OUTPUT_MAGIC {
        return Err("Typstワーカーの出力形式が不正です。".to_string());
    }
    let is_pdf = match output[4] {
        0 => false,
        1 => true,
        _ => return Err("Typstワーカーの出力種別が不正です。".to_string()),
    };
    if is_pdf != (format == TypstOutputFormat::Pdf) {
        return Err("Typstワーカーの出力形式が要求と一致しません。".to_string());
    }

    let mut offset = 5;
    let content = if is_pdf {
        let pdf = read_worker_bytes(&output, &mut offset)?;
        if pdf.is_empty() || pdf.len() > MAX_RENDERED_BYTES || !pdf.starts_with(b"%PDF-") {
            return Err("Typstワーカーが有効なPDFを出力しませんでした。".to_string());
        }
        TypstRenderContent::Pdf(pdf.to_vec())
    } else {
        TypstRenderContent::Pages(parse_worker_page_set(&output, &mut offset)?)
    };
    let preview_pages = parse_worker_page_set(&output, &mut offset)?;
    if preview_pages
        .iter()
        .any(|page| !page.starts_with(b"\x89PNG\r\n\x1a\n"))
    {
        return Err("TypstワーカーのPNGプレビューが不正です。".to_string());
    }
    if offset != output.len() {
        return Err("Typstワーカー出力の後ろに余分なデータがあります。".to_string());
    }
    Ok((content, preview_pages))
}

fn parse_worker_page_set(output: &[u8], offset: &mut usize) -> Result<Vec<Vec<u8>>, String> {
    let page_count = read_worker_u32(output, offset)?;
    if page_count == 0 || page_count > MAX_RENDERED_PAGES {
        return Err(format!(
            "Typstページ数は1から{MAX_RENDERED_PAGES}の範囲で指定してください。"
        ));
    }

    let mut pages = Vec::with_capacity(page_count);
    let mut total_bytes = 0usize;
    for _ in 0..page_count {
        let page = read_worker_bytes(output, offset)?;
        if page.is_empty() {
            return Err("Typstワーカーが空ページを出力しました。".to_string());
        }
        total_bytes = total_bytes.saturating_add(page.len());
        if total_bytes > MAX_RENDERED_BYTES {
            return Err("Typstページ出力の合計が上限を超えています。".to_string());
        }
        pages.push(page.to_vec());
    }
    Ok(pages)
}

fn read_worker_u32(output: &[u8], offset: &mut usize) -> Result<usize, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "Typstワーカー出力が不正です。".to_string())?;
    let bytes = output
        .get(*offset..end)
        .ok_or_else(|| "Typstワーカー出力が途中で切れています。".to_string())?;
    *offset = end;
    Ok(u32::from_be_bytes(bytes.try_into().unwrap()) as usize)
}

fn read_worker_bytes<'a>(output: &'a [u8], offset: &mut usize) -> Result<&'a [u8], String> {
    let length = read_worker_u32(output, offset)?;
    let end = offset
        .checked_add(length)
        .ok_or_else(|| "Typstワーカー出力が不正です。".to_string())?;
    let bytes = output
        .get(*offset..end)
        .ok_or_else(|| "Typstワーカー出力が途中で切れています。".to_string())?;
    *offset = end;
    Ok(bytes)
}

async fn read_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: usize,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > max_bytes {
            return Err(std::io::Error::other("worker output exceeded its limit"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn read_worker_diagnostics<R: AsyncRead + Unpin>(mut reader: R) -> Vec<u8> {
    let mut output = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let read = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return output,
            Ok(read) => read,
        };
        let remaining = MAX_DIAGNOSTIC_BYTES.saturating_sub(output.len());
        output.extend_from_slice(&buffer[..read.min(remaining)]);
    }
}

fn log_worker_diagnostics(stage: &str, diagnostics: &[u8]) {
    let summary = String::from_utf8_lossy(diagnostics)
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join(" | ");
    if summary.is_empty() {
        warn!("Typst worker failed at {stage}; no worker stage was reported");
    } else {
        warn!("Typst worker failed at {stage}; worker stages: {summary}");
    }
}

#[cfg(target_os = "linux")]
fn worker_command(executable: &std::path::Path) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--quiet",
        "--wait",
        "--pipe",
        "--collect",
        "--service-type=exec",
        "--setenv=RAYON_NUM_THREADS=1",
        "--unit",
        "nelfie-typst-worker.service",
        "--property=CPUQuota=100%",
        "--property=MemoryMax=1G",
        "--property=MemorySwapMax=0",
        "--property=RuntimeMaxSec=120s",
        "--property=TasksMax=8",
        "--property=KillMode=control-group",
        "--property=NoNewPrivileges=yes",
        "--property=PrivateTmp=yes",
        "--property=PrivateDevices=yes",
        "--property=ProtectSystem=strict",
        "--property=ProtectHome=read-only",
        "--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6",
        "--",
    ]);
    command.arg(executable).arg("--typst-worker");
    command
}

#[cfg(windows)]
fn worker_command(executable: &std::path::Path) -> Command {
    let mut command = Command::new(executable);
    command
        .arg("--typst-worker")
        .env_clear()
        .env("RAYON_NUM_THREADS", "1");
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    if let Some(windir) = std::env::var_os("WINDIR") {
        command.env("WINDIR", windir);
    }
    command
}

#[cfg(not(any(target_os = "linux", windows)))]
fn worker_command(_executable: &std::path::Path) -> Command {
    Command::new("nelfie-typst-isolation-not-supported-on-this-platform")
}

#[cfg(windows)]
mod windows_job {
    use std::{io, mem::size_of};

    use tokio::process::Child;
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::{
            JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
                JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP, JOB_OBJECT_LIMIT_JOB_MEMORY,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_PROCESS_MEMORY,
                JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        },
    };

    pub struct JobGuard(usize);

    impl Drop for JobGuard {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0 as HANDLE);
            }
        }
    }

    pub fn apply(child: &Child) -> io::Result<JobGuard> {
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let guard = JobGuard(job as usize);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        limits.ProcessMemoryLimit = 1024 * 1024 * 1024;
        limits.JobMemoryLimit = 1024 * 1024 * 1024;
        let limit_set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if limit_set == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut cpu = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        cpu.ControlFlags =
            JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
        cpu.Anonymous.CpuRate = 10_000;
        let cpu_set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                &cpu as *const _ as *const _,
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        };
        if cpu_set == 0 {
            return Err(io::Error::last_os_error());
        }

        let process_id = child
            .id()
            .ok_or_else(|| io::Error::other("worker process ID is unavailable"))?;
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, process_id) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let assigned = unsafe { AssignProcessToJobObject(job, process) };
        unsafe {
            CloseHandle(process);
        }
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(guard)
    }
}

pub fn is_worker_process() -> bool {
    std::env::args_os().any(|argument| argument == "--typst-worker")
}

pub fn clear_worker_environment() {
    let variables = std::env::vars_os()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    for name in variables {
        let name_text = name.to_string_lossy();
        if name_text.eq_ignore_ascii_case("SystemRoot") || name_text.eq_ignore_ascii_case("WINDIR")
        {
            continue;
        }
        // The worker calls this before any runtime or worker thread is started.
        unsafe { std::env::remove_var(name) };
    }
    // Rayon reads this when its global pool is first initialized.
    unsafe { std::env::set_var("RAYON_NUM_THREADS", "1") };
}

pub fn run_worker_stdio() -> Result<(), String> {
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err("このOSではTypstワーカーのリソース制限に対応していません。".to_string());

    eprintln!("typst-worker stage=reading-input");
    let mut input = Vec::new();
    std::io::stdin()
        .take((MAX_WORKER_JSON_BYTES + 1) as u64)
        .read_to_end(&mut input)
        .map_err(|error| format!("ワーカー入力を読み込めません: {error}"))?;
    if input.len() > MAX_WORKER_JSON_BYTES {
        return Err("ワーカー入力が72 MiBを超えています。".to_string());
    }
    eprintln!("typst-worker stage=input-received bytes={}", input.len());
    let request: WorkerRequest = serde_json::from_slice(&input)
        .map_err(|error| format!("ワーカー入力が不正です: {error}"))?;
    eprintln!("typst-worker stage=compiling format={:?}", request.format);
    let (output, warnings) = compile_worker_request(request)?;
    for warning in warnings.into_iter().take(64) {
        let _ = writeln!(std::io::stderr(), "warning: {warning}");
    }
    write_worker_output(output)
}

enum WorkerOutput {
    Pdf {
        pdf: Vec<u8>,
        preview_pages: Vec<Vec<u8>>,
    },
    Pages {
        pages: Vec<Vec<u8>>,
        preview_pages: Vec<Vec<u8>>,
    },
}

fn write_worker_output(output: WorkerOutput) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(WORKER_OUTPUT_MAGIC)
        .map_err(|error| format!("Typst出力ヘッダーを書き出せません: {error}"))?;
    match output {
        WorkerOutput::Pdf { pdf, preview_pages } => {
            stdout
                .write_all(&[1])
                .map_err(|error| format!("Typst出力種別を書き出せません: {error}"))?;
            write_worker_bytes(&mut stdout, &pdf, "PDF")?;
            write_worker_pages(&mut stdout, &preview_pages, "PNGプレビュー")?;
        }
        WorkerOutput::Pages {
            pages,
            preview_pages,
        } => {
            stdout
                .write_all(&[0])
                .map_err(|error| format!("Typst出力種別を書き出せません: {error}"))?;
            write_worker_pages(&mut stdout, &pages, "ページ")?;
            write_worker_pages(&mut stdout, &preview_pages, "PNGプレビュー")?;
        }
    }
    stdout
        .flush()
        .map_err(|error| format!("Typst出力を完了できません: {error}"))
}

fn write_worker_bytes(output: &mut impl Write, bytes: &[u8], label: &str) -> Result<(), String> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| format!("Typst {label}の出力サイズが大きすぎます。"))?;
    output
        .write_all(&length.to_be_bytes())
        .and_then(|_| output.write_all(bytes))
        .map_err(|error| format!("Typst {label}を書き出せません: {error}"))
}

fn write_worker_pages(
    output: &mut impl Write,
    pages: &[Vec<u8>],
    label: &str,
) -> Result<(), String> {
    let page_count = u32::try_from(pages.len())
        .map_err(|_| format!("Typst {label}のページ数が大きすぎます。"))?;
    output
        .write_all(&page_count.to_be_bytes())
        .map_err(|error| format!("Typst {label}のページ数を書き出せません: {error}"))?;
    for page in pages {
        write_worker_bytes(output, page, label)?;
    }
    Ok(())
}

fn compile_worker_request(request: WorkerRequest) -> Result<(WorkerOutput, Vec<String>), String> {
    if !is_safe_filename(&request.source_filename)
        || !request
            .source_filename
            .to_ascii_lowercase()
            .ends_with(".typ")
    {
        return Err("メインソースは安全な`.typ`ファイル名にしてください。".to_string());
    }
    if request.assets.len() > MAX_ASSETS {
        return Err(format!("Typst素材は{MAX_ASSETS}ファイルまでです。"));
    }

    let source = STANDARD
        .decode(request.source)
        .map_err(|_| "Typstソースのデータ形式が不正です。".to_string())?;
    if source.len() > MAX_FILE_BYTES {
        return Err("Typstソースは25 MiBを超えています。".to_string());
    }
    let mut files = HashMap::<String, Vec<u8>>::new();
    let mut lower_names = HashSet::new();
    let mut total = source.len();
    lower_names.insert(request.source_filename.to_ascii_lowercase());
    files.insert(request.source_filename.clone(), source);
    for asset in request.assets {
        if !is_safe_filename(&asset.filename) {
            return Err(format!(
                "安全でない素材ファイル名です: `{}`",
                asset.filename
            ));
        }
        if !lower_names.insert(asset.filename.to_ascii_lowercase()) {
            return Err(format!(
                "同名の素材ファイルがあります: `{}`",
                asset.filename
            ));
        }
        let bytes = STANDARD
            .decode(asset.data)
            .map_err(|_| format!("`{}` のデータ形式が不正です。", asset.filename))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!("`{}` は25 MiBを超えています。", asset.filename));
        }
        total = total.saturating_add(bytes.len());
        files.insert(asset.filename, bytes);
    }
    if total > MAX_TOTAL_INPUT_BYTES {
        return Err("Typst入力ファイルの合計は50 MiBまでです。".to_string());
    }

    let world = InMemoryWorld::new(request.source_filename, files)?;
    eprintln!("typst-worker stage=world-ready");
    let typst::diag::Warned { output, warnings } = typst::compile::<PagedDocument>(&world);
    eprintln!("typst-worker stage=typst-compiled");
    let document = output.map_err(|errors| {
        errors
            .iter()
            .take(20)
            .map(|diagnostic| diagnostic.message.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    if document.pages().is_empty() || document.pages().len() > MAX_RENDERED_PAGES {
        return Err(format!(
            "ページ画像は1から{MAX_RENDERED_PAGES}ページまで生成できます。"
        ));
    }

    let options = typst_svg::SvgOptions::default();
    let mut preview_total = 0usize;
    let mut output_total = 0usize;
    let mut preview_pages = Vec::with_capacity(document.pages().len());
    let mut output_pages = Vec::with_capacity(document.pages().len());
    for page in document.pages() {
        eprintln!(
            "typst-worker stage=rendering-page number={}",
            preview_pages.len() + 1
        );
        let svg = typst_svg::svg(page, &options);
        let preview = render_svg_to_png(&svg)?;
        preview_total = preview_total.saturating_add(preview.len());
        if preview_total > MAX_RENDERED_BYTES {
            return Err("PNGプレビューの合計が18 MiBを超えています。".to_string());
        }

        match request.format {
            TypstOutputFormat::Png => {
                output_total = output_total.saturating_add(preview.len());
                output_pages.push(preview.clone());
            }
            TypstOutputFormat::Svg => {
                let svg = svg.into_bytes();
                output_total = output_total.saturating_add(svg.len());
                output_pages.push(svg);
            }
            TypstOutputFormat::Pdf => {}
        }
        if output_total > MAX_RENDERED_BYTES {
            return Err("生成されたページ画像の合計が18 MiBを超えています。".to_string());
        }
        preview_pages.push(preview);
        eprintln!(
            "typst-worker stage=page-rendered number={}",
            preview_pages.len()
        );
    }

    let rendered = match request.format {
        TypstOutputFormat::Pdf => {
            let pdf =
                typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default()).map_err(|errors| {
                    errors
                        .iter()
                        .take(20)
                        .map(|diagnostic| diagnostic.message.to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                })?;
            if pdf.len() > MAX_RENDERED_BYTES {
                return Err("生成されたPDFが18 MiBを超えています。".to_string());
            }
            WorkerOutput::Pdf { pdf, preview_pages }
        }
        TypstOutputFormat::Png | TypstOutputFormat::Svg => WorkerOutput::Pages {
            pages: output_pages,
            preview_pages,
        },
    };
    Ok((
        rendered,
        warnings
            .into_iter()
            .map(|warning| warning.message.to_string())
            .collect(),
    ))
}

fn render_svg_to_png(svg: &str) -> Result<Vec<u8>, String> {
    let options = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_str(svg, &options)
        .map_err(|error| format!("Typst SVGをPNGに変換できません: {error}"))?;
    let size = tree.size();
    let width = (size.width() * PNG_SCALE).ceil();
    let height = (size.height() * PNG_SCALE).ceil();
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > MAX_PNG_DIMENSION
        || height > MAX_PNG_DIMENSION
        || (width as u64).saturating_mul(height as u64) > MAX_PNG_PIXELS
    {
        return Err("PNGページの寸法が上限を超えています。".to_string());
    }
    let width = width as u32;
    let height = height as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "PNGページ用のメモリを確保できません。".to_string())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(PNG_SCALE, PNG_SCALE),
        &mut pixmap.as_mut(),
    );
    pixmap
        .encode_png()
        .map_err(|error| format!("PNGをエンコードできません: {error}"))
}

struct InMemoryWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    main: FileId,
    files: HashMap<String, Bytes>,
    sources: Mutex<HashMap<FileId, Source>>,
    fonts: Vec<Font>,
    packages: Mutex<HashMap<PackageSpec, HashMap<String, Bytes>>>,
    package_http: reqwest::blocking::Client,
}

impl InMemoryWorld {
    fn new(main_filename: String, files: HashMap<String, Vec<u8>>) -> Result<Self, String> {
        let main_path = VirtualPath::new(main_filename.as_str())
            .map_err(|error| format!("メインファイル名が不正です: {error}"))?;
        let main = FileId::new(RootedPath::new(VirtualRoot::Project, main_path));
        let mut fonts = Vec::new();
        for data in typst_assets::fonts().chain(std::iter::once(
            include_bytes!("../assets/fonts/NotoSansJP.ttf").as_slice(),
        )) {
            fonts.extend(Font::iter(Bytes::new(data.to_vec())));
        }
        let font_bytes = files
            .iter()
            .filter(|(filename, _)| is_font_file(filename))
            .collect::<Vec<_>>();
        for (filename, data) in font_bytes {
            let found = Font::iter(Bytes::new(data.clone())).collect::<Vec<_>>();
            if found.is_empty() {
                return Err(format!("`{filename}` からフォントを読み込めません。"));
            }
            fonts.extend(found);
        }
        let book = FontBook::from_fonts(&fonts);
        let files = files
            .into_iter()
            .map(|(filename, bytes)| (filename, Bytes::new(bytes)))
            .collect();
        let package_http = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(StdDuration::from_secs(30))
            .build()
            .map_err(|error| format!("パッケージ取得クライアントを作成できません: {error}"))?;

        Ok(Self {
            library: LazyHash::new(Library::builder().build()),
            book: LazyHash::new(book),
            main,
            files,
            sources: Mutex::new(HashMap::new()),
            fonts,
            packages: Mutex::new(HashMap::new()),
            package_http,
        })
    }

    fn package_file(&self, package: &PackageSpec, path: &str) -> Result<Bytes, FileError> {
        if package.namespace.as_str() != "preview" {
            return Err(file_error(
                "@preview以外のTypstパッケージは禁止されています。",
            ));
        }
        let mut cache = self
            .packages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !cache.contains_key(package) {
            if cache.len() >= MAX_PACKAGES_PER_RENDER {
                return Err(file_error("Typstパッケージ数の上限を超えました。"));
            }
            let files = self.download_package(package).map_err(file_error)?;
            cache.insert(package.clone(), files);
        }
        cache
            .get(package)
            .and_then(|files| files.get(path))
            .cloned()
            .ok_or_else(|| file_error("Typstパッケージ内に指定ファイルがありません。"))
    }

    fn download_package(&self, package: &PackageSpec) -> Result<HashMap<String, Bytes>, String> {
        eprintln!(
            "typst-worker stage=downloading-package package=@{}/{}:{}",
            package.namespace, package.name, package.version
        );
        let url = format!(
            "https://packages.typst.org/preview/{}-{}.tar.gz",
            package.name, package.version
        );
        let response = self
            .package_http
            .get(&url)
            .send()
            .map_err(|error| format!("公式Typstパッケージを取得できません: {error}"))?;
        if response.url().host_str() != Some("packages.typst.org")
            || response.url().scheme() != "https"
            || response.status().is_redirection()
        {
            return Err("Typstパッケージ取得先が許可されていません。".to_string());
        }
        if !response.status().is_success() {
            return Err(format!(
                "Typstパッケージ取得に失敗しました（HTTP {}）。",
                response.status()
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PACKAGE_ARCHIVE_BYTES as u64)
        {
            return Err("Typstパッケージが25 MiBを超えています。".to_string());
        }
        let mut archive_bytes = Vec::new();
        response
            .take((MAX_PACKAGE_ARCHIVE_BYTES + 1) as u64)
            .read_to_end(&mut archive_bytes)
            .map_err(|error| format!("Typstパッケージを読み込めません: {error}"))?;
        if archive_bytes.len() > MAX_PACKAGE_ARCHIVE_BYTES {
            return Err("Typstパッケージが25 MiBを超えています。".to_string());
        }

        let decoder = GzDecoder::new(archive_bytes.as_slice());
        let mut archive = Archive::new(decoder);
        let entries = archive
            .entries()
            .map_err(|error| format!("Typstパッケージアーカイブが不正です: {error}"))?;
        let mut files = HashMap::new();
        let mut total_bytes = 0usize;
        let mut count = 0usize;
        for entry in entries {
            count += 1;
            if count > MAX_PACKAGE_FILES {
                return Err("Typstパッケージ内のファイル数が上限を超えました。".to_string());
            }
            let entry =
                entry.map_err(|error| format!("Typstパッケージを読み込めません: {error}"))?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path_bytes = entry.path_bytes();
            let path = std::str::from_utf8(&path_bytes)
                .map_err(|_| "TypstパッケージにUTF-8でないパスがあります。".to_string())?
                .to_string();
            if !is_safe_package_path(&path) {
                return Err("Typstパッケージに安全でないパスがあります。".to_string());
            }
            let size = usize::try_from(entry.size())
                .map_err(|_| "Typstパッケージ内のファイルサイズが不正です。".to_string())?;
            total_bytes = total_bytes.saturating_add(size);
            if size > MAX_FILE_BYTES || total_bytes > MAX_PACKAGE_EXPANDED_BYTES {
                return Err("Typstパッケージの展開サイズが上限を超えました。".to_string());
            }
            let mut bytes = Vec::with_capacity(size);
            entry
                .take((MAX_FILE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| format!("Typstパッケージ内のファイルを読めません: {error}"))?;
            if bytes.len() != size {
                return Err("Typstパッケージ内のファイルサイズが一致しません。".to_string());
            }
            files.insert(path, Bytes::new(bytes));
        }
        Ok(files)
    }
}

impl World for InMemoryWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> Result<Source, FileError> {
        let mut sources = self
            .sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(source) = sources.get(&id) {
            return Ok(source.clone());
        }
        let bytes = self.file(id)?;
        let text = std::str::from_utf8(bytes.as_slice()).map_err(|_| FileError::InvalidUtf8)?;
        let source = Source::new(id, text.to_string());
        sources.insert(id, source.clone());
        Ok(source)
    }

    fn file(&self, id: FileId) -> Result<Bytes, FileError> {
        let path = id.vpath().get_without_slash();
        match id.root() {
            VirtualRoot::Project => self
                .files
                .get(path)
                .cloned()
                .ok_or_else(|| FileError::NotFound(PathBuf::from(path))),
            VirtualRoot::Package(package) => self.package_file(package, path),
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        let now = chrono::Utc::now();
        let now = match offset {
            Some(offset) => now.checked_add_signed(chrono::Duration::milliseconds(
                (offset.seconds() * 1000.0).round() as i64,
            ))?,
            None => now,
        };
        Datetime::from_ymd(now.year(), now.month() as u8, now.day() as u8)
    }
}

fn file_error(message: impl Into<String>) -> FileError {
    FileError::Other(Some(message.into().into()))
}

fn is_font_file(filename: &str) -> bool {
    matches!(
        filename
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .as_deref(),
        Some("ttf" | "otf" | "ttc" | "otc")
    )
}

fn is_safe_package_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path
            .chars()
            .any(|character| matches!(character, '\\' | ':' | '\0'))
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_output(byte: u8) -> TypstRenderOutput {
        TypstRenderOutput {
            content: TypstRenderContent::Pdf(vec![byte]),
            preview_pages: vec![vec![byte]],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn publish_selects_the_exact_reviewed_draft_id() {
        let store = TypstDraftStore::default();
        let (first_id, _) = store
            .insert(TypstOutputFormat::Pdf, 0, test_output(1))
            .unwrap();
        let (second_id, _) = store
            .insert(TypstOutputFormat::Svg, 0, test_output(2))
            .unwrap();

        assert_ne!(first_id, second_id);
        assert!(store.take_reviewed(&first_id, 1).is_err());
        store.mark_preview_delivered(&first_id, 0);
        assert!(store.take_reviewed(&second_id, 1).is_err());

        let (format, content, _) = store.take_reviewed(&first_id, 1).unwrap();
        assert_eq!(format, TypstOutputFormat::Pdf);
        assert!(matches!(content, TypstRenderContent::Pdf(bytes) if bytes == vec![1]));
        assert!(store.take_reviewed(&first_id, 2).is_err());

        store.mark_preview_delivered(&second_id, 0);
        let (_, content, _) = store.take_reviewed(&second_id, 1).unwrap();
        assert!(matches!(content, TypstRenderContent::Pdf(bytes) if bytes == vec![2]));
    }
}
