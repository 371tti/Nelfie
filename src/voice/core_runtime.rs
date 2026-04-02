use std::{
    collections::{HashMap, HashSet},
    io::{Cursor, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use flate2::read::GzDecoder;
use log::{info, warn};
use reqwest::{
    Client as HttpClient,
    header::{ACCEPT, AUTHORIZATION, USER_AGENT},
};
use serde::Deserialize;
use voicevox_core::{
    AccelerationMode, AudioQuery, CharacterMeta, StyleId,
    nonblocking::{Onnxruntime, OpenJtalk, Synthesizer, VoiceModelFile},
};

use super::types::VoiceCoreConfig;

const OPEN_JTALK_REPO: &str = "r9y9/open_jtalk";
const OPEN_JTALK_TAG: &str = "v1.11.1";
const OPEN_JTALK_ASSET_NAME: &str = "open_jtalk_dic_utf_8-1.11.tar.gz";

const ONNXRUNTIME_BUILDER_REPO: &str = "VOICEVOX/onnxruntime-builder";

const VOICEVOX_VVM_REPO: &str = "VOICEVOX/voicevox_vvm";
const MODELS_DIR_NAME: &str = "vvms";
const MODELS_TERMS_FILE: &str = "TERMS.txt";
const MODELS_README_FILE: &str = "README.txt";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug)]
struct DiscoveredModel {
    model_path: PathBuf,
    style_ids: Vec<u32>,
    version_key: String,
}

pub(super) struct CoreRuntime {
    synthesizer: Synthesizer<OpenJtalk>,
    style_to_model: HashMap<u32, PathBuf>,
    output_sampling_rate: u32,
}

impl CoreRuntime {
    // レート制限かかるなら
    fn github_token() -> Option<String> {
        std::env::var("GH_TOKEN")
            .ok()
            .or_else(|| std::env::var("GITHUB_TOKEN").ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    fn github_http_client() -> Result<HttpClient, String> {
        HttpClient::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))
    }

    async fn github_api_get<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
        let client = Self::github_http_client()?;
        let mut req = client
            .get(url)
            .header(USER_AGENT, "observer-rust/voicevox-assets")
            .header(ACCEPT, "application/vnd.github+json");

        if let Some(token) = Self::github_token() {
            req = req.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("GitHub API request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(format!(
                "GitHub API request failed with status {} for '{}': {}",
                status,
                url,
                body.chars().take(400).collect::<String>()
            ));
        }

        response
            .json::<T>()
            .await
            .map_err(|e| format!("failed to decode GitHub API response: {e}"))
    }

    async fn fetch_release_by_tag(repo: &str, tag: &str) -> Result<GithubRelease, String> {
        let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
        Self::github_api_get(&url).await
    }

    async fn fetch_latest_release(repo: &str) -> Result<GithubRelease, String> {
        let url = format!("https://api.github.com/repos/{repo}/releases/latest");
        Self::github_api_get(&url).await
    }

    async fn download_asset_bytes(url: &str) -> Result<Vec<u8>, String> {
        let client = Self::github_http_client()?;
        let mut req = client
            .get(url)
            .header(USER_AGENT, "observer-rust/voicevox-assets")
            .header(ACCEPT, "application/octet-stream");

        if let Some(token) = Self::github_token() {
            req = req.header(AUTHORIZATION, format!("Bearer {}", token));
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("asset download request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<no body>".to_string());
            return Err(format!(
                "asset download failed with status {} for '{}': {}",
                status,
                url,
                body.chars().take(400).collect::<String>()
            ));
        }

        response
            .bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("failed to read downloaded asset bytes: {e}"))
    }

    fn extract_tgz_strip_first_dir(archive: &[u8], output_root: &Path) -> Result<(), String> {
        let mut tar = tar::Archive::new(GzDecoder::new(Cursor::new(archive)));

        let entries = tar
            .entries()
            .map_err(|e| format!("failed to read tgz entries: {e}"))?;

        for entry in entries {
            let mut entry = entry.map_err(|e| format!("failed to read tgz entry: {e}"))?;

            if !entry.header().entry_type().is_file() {
                continue;
            }

            let raw_path = entry
                .path()
                .map_err(|e| format!("failed to read tgz entry path: {e}"))?
                .into_owned();

            let stripped = raw_path.components().skip(1).collect::<PathBuf>();
            if stripped.as_os_str().is_empty() {
                continue;
            }

            let dst = output_root.join(stripped);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "failed to create extraction directory '{}': {e}",
                        parent.display()
                    )
                })?;
            }

            let mut content = Vec::new();
            entry
                .read_to_end(&mut content)
                .map_err(|e| format!("failed to read tgz entry bytes: {e}"))?;
            std::fs::write(&dst, content)
                .map_err(|e| format!("failed to write extracted file '{}': {e}", dst.display()))?;
        }

        Ok(())
    }

    fn select_onnxruntime_asset_name(release: &GithubRelease) -> Option<String> {
        let mut preferred_prefixes: Vec<&str> = match (std::env::consts::OS, std::env::consts::ARCH)
        {
            ("windows", "x86_64") => vec![
                "voicevox_onnxruntime-win-x64-dml-",
                "voicevox_onnxruntime-win-x64-",
            ],
            ("windows", "x86") => vec!["voicevox_onnxruntime-win-x86-"],
            ("linux", "x86_64") => vec![
                "voicevox_onnxruntime-linux-x64-",
                "voicevox_onnxruntime-linux-x64-cuda-",
            ],
            ("linux", "aarch64") => vec!["voicevox_onnxruntime-linux-arm64-"],
            ("macos", "x86_64") => vec!["voicevox_onnxruntime-osx-x86_64-"],
            ("macos", "aarch64") => vec!["voicevox_onnxruntime-osx-arm64-"],
            _ => vec![],
        };

        if preferred_prefixes.is_empty() {
            preferred_prefixes.push("voicevox_onnxruntime-");
        }

        for prefix in preferred_prefixes {
            if let Some(asset) = release.assets.iter().find(|asset| {
                asset.name.starts_with(prefix)
                    && (asset.name.ends_with(".tgz") || asset.name.ends_with(".tar.gz"))
            }) {
                return Some(asset.name.clone());
            }
        }

        None
    }

    async fn ensure_voicevox_onnxruntime(config: &VoiceCoreConfig) -> Result<(), String> {
        let configured_path = PathBuf::from(&config.onnxruntime_filename);
        if configured_path.is_file() {
            return Ok(());
        }

        let lib_dir = configured_path.parent().ok_or_else(|| {
            format!(
                "invalid ONNX Runtime path '{}': parent directory is missing",
                configured_path.display()
            )
        })?;

        let onnxruntime_root = lib_dir.parent().ok_or_else(|| {
            format!(
                "invalid ONNX Runtime path '{}': expected '<root>/lib/<dll>'",
                configured_path.display()
            )
        })?;

        std::fs::create_dir_all(onnxruntime_root).map_err(|e| {
            format!(
                "failed to create ONNX Runtime root directory '{}': {e}",
                onnxruntime_root.display()
            )
        })?;

        info!(
            "VOICEVOX ONNX Runtime not found at '{}'; downloading from '{}'",
            configured_path.display(),
            ONNXRUNTIME_BUILDER_REPO
        );

        let release = Self::fetch_latest_release(ONNXRUNTIME_BUILDER_REPO).await?;
        let asset_name = Self::select_onnxruntime_asset_name(&release).ok_or_else(|| {
            format!(
                "no matching ONNX Runtime archive for current platform in release '{}'",
                release.tag_name
            )
        })?;

        let asset = release
            .assets
            .iter()
            .find(|a| a.name == asset_name)
            .ok_or_else(|| {
                format!(
                    "failed to find asset '{}' in release '{}'",
                    asset_name, release.tag_name
                )
            })?;

        let archive = Self::download_asset_bytes(&asset.browser_download_url).await?;
        Self::extract_tgz_strip_first_dir(&archive, onnxruntime_root)?;

        if !configured_path.is_file() {
            return Err(format!(
                "ONNX Runtime download finished but '{}' is still missing",
                configured_path.display()
            ));
        }

        info!(
            "VOICEVOX ONNX Runtime downloaded successfully (tag={}, asset={})",
            release.tag_name, asset_name
        );

        Ok(())
    }

    fn has_open_jtalk_dictionary(dict_dir: &Path) -> Result<bool, String> {
        if !dict_dir.is_dir() {
            return Ok(false);
        }

        let mut has_dic = false;
        for entry in std::fs::read_dir(dict_dir).map_err(|e| {
            format!(
                "failed to read dict directory '{}': {e}",
                dict_dir.display()
            )
        })? {
            let entry = entry.map_err(|e| {
                format!(
                    "failed to read an entry in dict directory '{}': {e}",
                    dict_dir.display()
                )
            })?;

            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("dic"))
                .unwrap_or(false)
            {
                has_dic = true;
                break;
            }
        }

        Ok(has_dic)
    }

    fn has_vvm_assets(vvm_dir: &Path) -> Result<bool, String> {
        if !vvm_dir.is_dir() {
            return Ok(false);
        }

        for entry in std::fs::read_dir(vvm_dir)
            .map_err(|e| format!("failed to read VVM directory '{}': {e}", vvm_dir.display()))?
        {
            let entry = entry.map_err(|e| {
                format!(
                    "failed to read an entry in VVM directory '{}': {e}",
                    vvm_dir.display()
                )
            })?;

            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("vvm"))
                .unwrap_or(false)
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    async fn ensure_open_jtalk_dictionary(dict_dir: &Path) -> Result<(), String> {
        if Self::has_open_jtalk_dictionary(dict_dir)? {
            return Ok(());
        }

        let dict_root = dict_dir.parent().ok_or_else(|| {
            format!(
                "invalid OpenJTalk dictionary path '{}': parent directory is missing",
                dict_dir.display()
            )
        })?;

        std::fs::create_dir_all(dict_root).map_err(|e| {
            format!(
                "failed to create dictionary root directory '{}': {e}",
                dict_root.display()
            )
        })?;

        info!(
            "OpenJTalk dictionary not found at '{}'; downloading from GitHub release assets",
            dict_dir.display()
        );

        let release = Self::fetch_release_by_tag(OPEN_JTALK_REPO, OPEN_JTALK_TAG).await?;
        let asset = release
            .assets
            .iter()
            .find(|asset| asset.name == OPEN_JTALK_ASSET_NAME)
            .ok_or_else(|| {
                format!(
                    "failed to find '{}' in release '{}' of '{}'",
                    OPEN_JTALK_ASSET_NAME, release.tag_name, OPEN_JTALK_REPO
                )
            })?;

        let archive = Self::download_asset_bytes(&asset.browser_download_url).await?;
        let mut tar = tar::Archive::new(GzDecoder::new(Cursor::new(archive)));
        tar.unpack(dict_root).map_err(|e| {
            format!(
                "failed to extract OpenJTalk dictionary archive into '{}': {e}",
                dict_root.display()
            )
        })?;

        if !Self::has_open_jtalk_dictionary(dict_dir)? {
            return Err(format!(
                "OpenJTalk dictionary download finished but '{}' is still unavailable",
                dict_dir.display()
            ));
        }

        info!(
            "OpenJTalk dictionary downloaded successfully (tag={})",
            release.tag_name
        );

        Ok(())
    }

    async fn ensure_vvm_models(vvm_dir: &Path) -> Result<(), String> {
        if Self::has_vvm_assets(vvm_dir)? {
            return Ok(());
        }

        std::fs::create_dir_all(vvm_dir).map_err(|e| {
            format!(
                "failed to create VVM directory '{}': {e}",
                vvm_dir.display()
            )
        })?;

        let models_root = vvm_dir.parent().unwrap_or(vvm_dir);

        info!(
            "VVM assets not found at '{}'; downloading from GitHub release assets",
            vvm_dir.display()
        );

        let release = Self::fetch_latest_release(VOICEVOX_VVM_REPO).await?;
        let mut downloaded = 0usize;

        for asset in &release.assets {
            let lower = asset.name.to_ascii_lowercase();
            if !lower.ends_with(".vvm") {
                continue;
            }

            let bytes = Self::download_asset_bytes(&asset.browser_download_url).await?;
            let out_path = vvm_dir.join(&asset.name);
            std::fs::write(&out_path, bytes)
                .map_err(|e| format!("failed to write VVM file '{}': {e}", out_path.display()))?;
            downloaded += 1;
        }

        if let Some(readme) = release.assets.iter().find(|a| a.name == MODELS_README_FILE) {
            let bytes = Self::download_asset_bytes(&readme.browser_download_url).await?;
            let out_path = models_root.join(MODELS_README_FILE);
            std::fs::write(&out_path, bytes).map_err(|e| {
                format!(
                    "failed to write models README '{}': {e}",
                    out_path.display()
                )
            })?;
        }

        if let Some(terms) = release.assets.iter().find(|a| a.name == MODELS_TERMS_FILE) {
            let bytes = Self::download_asset_bytes(&terms.browser_download_url).await?;
            let out_path = models_root.join(MODELS_TERMS_FILE);
            std::fs::write(&out_path, bytes).map_err(|e| {
                format!("failed to write models terms '{}': {e}", out_path.display())
            })?;
        }

        if downloaded == 0 {
            return Err(format!(
                "no .vvm assets were found in latest release '{}' of '{}'",
                release.tag_name, VOICEVOX_VVM_REPO
            ));
        }

        if !Self::has_vvm_assets(vvm_dir)? {
            return Err(format!(
                "VVM download finished but '{}' is still unavailable",
                vvm_dir.display()
            ));
        }

        info!(
            "VVM assets downloaded successfully (tag={}, files={})",
            release.tag_name, downloaded
        );

        Ok(())
    }

    async fn ensure_required_assets(config: &VoiceCoreConfig) -> Result<(), String> {
        let dict_dir = PathBuf::from(&config.open_jtalk_dict_dir);
        let vvm_dir = PathBuf::from(&config.vvm_dir);

        Self::ensure_voicevox_onnxruntime(config).await?;
        Self::ensure_open_jtalk_dictionary(&dict_dir).await?;
        Self::ensure_vvm_models(&vvm_dir).await?;

        if vvm_dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| !name.eq_ignore_ascii_case(MODELS_DIR_NAME))
            .unwrap_or(false)
        {
            warn!(
                "VVM directory '{}' does not end with '{}'; ensure this is intentional",
                vvm_dir.display(),
                MODELS_DIR_NAME
            );
        }

        Ok(())
    }

    fn parse_acceleration_mode(mode: &str) -> AccelerationMode {
        match mode.trim().to_ascii_lowercase().as_str() {
            "auto" => {
                info!(
                    "VOICEVOX acceleration 'auto' is mapped to 'cpu' to avoid ORT mixed-provider warnings. Set VOICEVOX_CORE_ACCELERATION=gpu to force GPU."
                );
                AccelerationMode::Cpu
            }
            "cpu" => AccelerationMode::Cpu,
            "gpu" => AccelerationMode::Gpu,
            other => {
                warn!(
                    "unknown VOICEVOX acceleration mode '{}', fallback to auto",
                    other
                );
                AccelerationMode::Auto
            }
        }
    }

    async fn load_onnxruntime(config: &VoiceCoreConfig) -> Result<&'static Onnxruntime, String> {
        if let Some(existing) = Onnxruntime::get() {
            return Ok(existing);
        }

        let mut candidates = Vec::new();

        if !config.onnxruntime_filename.trim().is_empty() {
            candidates.push(config.onnxruntime_filename.clone());
        }

        let default_versioned = Onnxruntime::LIB_VERSIONED_FILENAME.to_string();
        if !candidates.iter().any(|c| c == &default_versioned) {
            candidates.push(default_versioned);
        }

        let default_unversioned = Onnxruntime::LIB_UNVERSIONED_FILENAME.to_string();
        if !candidates.iter().any(|c| c == &default_unversioned) {
            candidates.push(default_unversioned);
        }

        let mut tried = Vec::new();
        for (idx, candidate) in candidates.into_iter().enumerate() {
            match Onnxruntime::load_once()
                .filename(&candidate)
                .perform()
                .await
            {
                Ok(ort) => {
                    if idx > 0 {
                        warn!(
                            "ONNX Runtime init fallback succeeded with filename='{}'",
                            candidate
                        );
                    }
                    return Ok(ort);
                }
                Err(e) => tried.push(format!("{} => {}", candidate, e)),
            }
        }

        Err(format!(
            "failed to initialize ONNX Runtime (checked candidates: {})",
            tried.join(" | ")
        ))
    }

    fn discover_vvm_files(vvm_dir: &Path) -> Result<Vec<PathBuf>, String> {
        if !vvm_dir.exists() {
            return Err(format!(
                "VVM directory does not exist: {}",
                vvm_dir.display()
            ));
        }
        if !vvm_dir.is_dir() {
            return Err(format!(
                "VVM path is not a directory: {}",
                vvm_dir.display()
            ));
        }

        let mut files = Vec::new();
        let entries = std::fs::read_dir(vvm_dir)
            .map_err(|e| format!("failed to read VVM directory '{}': {e}", vvm_dir.display()))?;

        for entry in entries {
            let entry = entry.map_err(|e| {
                format!(
                    "failed to read an entry in VVM directory '{}': {e}",
                    vvm_dir.display()
                )
            })?;

            let path = entry.path();
            if path
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("vvm"))
                .unwrap_or(false)
            {
                files.push(path);
            }
        }

        files.sort();

        if files.is_empty() {
            return Err(format!("no .vvm files found under '{}'", vvm_dir.display()));
        }

        Ok(files)
    }

    fn collect_style_ids(metas: &[CharacterMeta]) -> Vec<u32> {
        let mut ids = metas
            .iter()
            .flat_map(|ch| ch.styles.iter().map(|style| style.id.0))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    fn collect_version_key(metas: &[CharacterMeta]) -> String {
        let mut versions = metas
            .iter()
            .map(|ch| ch.version.0.clone())
            .collect::<Vec<_>>();
        versions.sort();
        versions.dedup();
        versions.join("|")
    }

    fn build_speaker_not_found_error(&self, speaker: u32) -> String {
        let mut styles = self.style_to_model.keys().copied().collect::<Vec<_>>();
        styles.sort_unstable();

        let preview_limit = 20usize;
        let preview = styles
            .iter()
            .take(preview_limit)
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");

        if styles.is_empty() {
            return format!(
                "speaker(style id) {} is not available: no styles were indexed from VVM files",
                speaker
            );
        }

        if styles.len() > preview_limit {
            return format!(
                "speaker(style id) {} is not available. available styles (first {} of {}): {}",
                speaker,
                preview_limit,
                styles.len(),
                preview
            );
        }

        format!(
            "speaker(style id) {} is not available. available styles: {}",
            speaker, preview
        )
    }

    pub(super) async fn new(config: &VoiceCoreConfig) -> Result<Self, String> {
        Self::ensure_required_assets(config).await?;

        let ort = Self::load_onnxruntime(config).await?;
        let acceleration = Self::parse_acceleration_mode(&config.acceleration_mode);

        let open_jtalk = OpenJtalk::new(&config.open_jtalk_dict_dir)
            .await
            .map_err(|e| {
                format!(
                    "failed to initialize OpenJTalk dictionary '{}': {}",
                    config.open_jtalk_dict_dir, e
                )
            })?;

        let synthesizer = Synthesizer::builder(ort)
            .text_analyzer(open_jtalk)
            .acceleration_mode(acceleration)
            .cpu_num_threads(config.cpu_threads)
            .build()
            .map_err(|e| format!("voicevox synthesizer build failed: {e}"))?;

        let vvm_dir = PathBuf::from(&config.vvm_dir);
        let vvm_files = Self::discover_vvm_files(&vvm_dir)?;

        let mut discovered = Vec::<DiscoveredModel>::new();
        let mut version_counts = HashMap::<String, usize>::new();

        for model_path in vvm_files {
            let model = VoiceModelFile::open(&model_path)
                .await
                .map_err(|e| format!("failed to open VVM '{}': {e}", model_path.display()))?;

            let style_ids = Self::collect_style_ids(model.metas());
            if style_ids.is_empty() {
                warn!(
                    "VVM '{}' has no styles in metadata; skipping",
                    model_path.display()
                );
                continue;
            }

            let version_key = Self::collect_version_key(model.metas());
            *version_counts.entry(version_key.clone()).or_insert(0) += 1;

            discovered.push(DiscoveredModel {
                model_path,
                style_ids,
                version_key,
            });
        }

        if discovered.is_empty() {
            return Err(format!(
                "no usable style IDs were found in VVM files under '{}'",
                config.vvm_dir
            ));
        }

        let selected_version = version_counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(version, _)| version.clone())
            .ok_or_else(|| "failed to determine model version set".to_string())?;

        if version_counts.len() > 1 {
            let details = version_counts
                .iter()
                .map(|(k, v)| format!("{}: {} files", k, v))
                .collect::<Vec<_>>()
                .join(", ");
            warn!(
                "detected mixed VVM character versions ({}). Using '{}' only to avoid voicevox_core meta mismatch warnings.",
                details, selected_version
            );
        }

        let mut style_to_model = HashMap::new();
        let mut selected_model_paths = HashSet::new();
        let mut skipped_models = 0usize;

        for discovered_model in discovered {
            if discovered_model.version_key != selected_version {
                skipped_models += 1;
                continue;
            }

            selected_model_paths.insert(discovered_model.model_path.clone());

            for style_id in &discovered_model.style_ids {
                if let Some(prev) =
                    style_to_model.insert(*style_id, discovered_model.model_path.clone())
                    && prev != discovered_model.model_path
                {
                    warn!(
                        "style id {} appears in multiple VVMs: '{}' and '{}'; using latest",
                        style_id,
                        prev.display(),
                        discovered_model.model_path.display()
                    );
                }
            }
        }

        if style_to_model.is_empty() {
            return Err(format!(
                "no usable style IDs remained after version filtering under '{}'",
                config.vvm_dir
            ));
        }

        if skipped_models > 0 {
            warn!(
                "skipped {} VVM file(s) due to version mismatch with selected '{}'",
                skipped_models, selected_version
            );
        }

        if !config.load_all_models {
            warn!(
                "VOICEVOX load_all_models=false is ignored in concurrent mode; preloading all selected VVMs"
            );
        }

        for model_path in &selected_model_paths {
            let model = VoiceModelFile::open(model_path).await.map_err(|e| {
                format!(
                    "failed to open VVM '{}' for loading: {e}",
                    model_path.display()
                )
            })?;

            synthesizer
                .load_voice_model(&model)
                .perform()
                .await
                .map_err(|e| format!("failed to load VVM '{}': {e}", model_path.display()))?;
        }

        info!(
            "VOICEVOX CORE initialized (acceleration={}, cpu_threads={}, load_all_models=forced(true), output_sampling_rate={}, output_stereo=fixed(false), dict_dir={}, vvm_dir={}, model_count={}, style_count={})",
            if config.acceleration_mode.trim().eq_ignore_ascii_case("auto") {
                "cpu(auto-mapped)".to_string()
            } else {
                config.acceleration_mode.clone()
            },
            config.cpu_threads,
            config.output_sampling_rate,
            config.open_jtalk_dict_dir,
            config.vvm_dir,
            selected_model_paths.len(),
            style_to_model.len()
        );

        Ok(Self {
            synthesizer,
            style_to_model,
            output_sampling_rate: config.output_sampling_rate,
        })
    }

    pub(super) async fn synthesize(
        &self,
        text: &str,
        speaker: u32,
        speed_scale: Option<f32>,
        pitch_scale: Option<f32>,
    ) -> Result<Vec<u8>, String> {
        if !self.style_to_model.contains_key(&speaker) {
            return Err(self.build_speaker_not_found_error(speaker));
        }

        let style_id = StyleId::new(speaker);
        let mut query: AudioQuery = self
            .synthesizer
            .create_audio_query(text, style_id)
            .await
            .map_err(|e| format!("failed to create audio query: {e}"))?;

        // VOICEVOXの推奨サンプリングレートを尊重する（警告抑止）。
        // Discord向け品質を優先して出力条件を固定する。
        query.output_sampling_rate = self.output_sampling_rate;
        query.output_stereo = false;

        if let Some(speed_scale) = speed_scale {
            query.speed_scale = speed_scale;
        }

        if let Some(pitch_scale) = pitch_scale {
            query.pitch_scale = pitch_scale;
        }

        self.synthesizer
            .synthesis(&query, style_id)
            .enable_interrogative_upspeak(true)
            .perform()
            .await
            .map_err(|e| format!("voice synthesis failed: {e}"))
    }
}
