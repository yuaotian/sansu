use anyhow::Result;
use rmcp::{model::*, Error as McpError};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::Client;
use ring::digest::{Context as ShaContext, SHA256};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use encoding_rs::{GBK, WINDOWS_1252, UTF_8};
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::types::{AcemcpRequest, AcemcpConfig, ProjectIndexStatus, ProjectsIndexStatus, IndexStatus};
use crate::log_debug;
use crate::log_important;

/// Acemcp工具实现
pub struct AcemcpTool;

impl AcemcpTool {
    /// 执行代码库搜索（仅搜索，不触发索引）
    pub async fn search_context(request: AcemcpRequest) -> Result<CallToolResult, McpError> {
        log_important!(info,
            "Acemcp搜索请求（仅搜索模式）: project_root_path={}, query={}",
            request.project_root_path, request.query
        );

        // 读取配置
        let mut acemcp_config = Self::get_acemcp_config()
            .await
            .map_err(|e| McpError::internal_error(format!("获取acemcp配置失败: {}", e), None))?;

        // 规范化 base_url（缺协议时补 http://），并去除末尾斜杠
        if let Some(base) = &acemcp_config.base_url {
            let normalized = normalize_base_url(base);
            acemcp_config.base_url = Some(normalized);
        }

        // 首次搜索时自动启动文件监听（如果尚未启动）
        let watcher_manager = super::watcher::get_watcher_manager();
        if !watcher_manager.is_watching(&request.project_root_path) {
            log_debug!("首次搜索，尝试启动文件监听");
            if let Err(e) = watcher_manager.start_watching(
                request.project_root_path.clone(),
                acemcp_config.clone()
            ).await {
                log_debug!("启动文件监听失败（不影响搜索）: {}", e);
            }
        }

        // 1. 检查初始索引状态
        let initial_state = get_initial_index_state(&request.project_root_path);
        log_debug!("项目索引状态: {:?}", initial_state);

        // 2. 根据状态执行相应操作
        let mut hint_message = String::new();
        match initial_state {
            InitialIndexState::Missing | InitialIndexState::Idle | InitialIndexState::Failed => {
                // 启动后台索引
                if let Err(e) = ensure_initial_index_background(&acemcp_config, &request.project_root_path).await {
                    log_debug!("启动后台索引失败（不影响搜索）: {}", e);
                } else {
                    hint_message = "\n\n💡 提示：当前项目索引尚未完全初始化，已在后台启动索引，稍后搜索结果会更完整。".to_string();
                }
            }
            InitialIndexState::Indexing => {
                // 正在索引中，应用智能等待
                if let Some((min_wait, max_wait)) = acemcp_config.smart_wait_range {
                    use rand::Rng;
                    let wait_secs = rand::thread_rng().gen_range(min_wait..=max_wait);

                    log_important!(info, "检测到索引正在进行中，智能等待 {} 秒后执行搜索", wait_secs);
                    tokio::time::sleep(tokio::time::Duration::from_secs(wait_secs)).await;

                    hint_message = format!("\n\n💡 提示：检测到索引正在进行中，已等待 {} 秒以获取更完整的搜索结果。", wait_secs);
                }
            }
            InitialIndexState::Synced => {
                // 已完成索引，直接搜索
                log_debug!("项目索引已完成，直接执行搜索");
            }
        }

        // 3. 执行搜索（不触发索引）
        let search_result = match search_only(&acemcp_config, &request.project_root_path, &request.query).await {
            Ok(text) => text,
            Err(e) => {
                return Ok(CallToolResult {
                    content: vec![Content::text(format!("Acemcp搜索失败: {}", e))],
                    is_error: Some(true)
                });
            }
        };

        // 4. 附加提示信息
        let final_result = if hint_message.is_empty() {
            search_result
        } else {
            format!("{}{}", search_result, hint_message)
        };

        Ok(CallToolResult { content: vec![Content::text(final_result)], is_error: None })
    }

    /// 执行索引更新（向后兼容的索引+搜索一体化接口）
    pub async fn index_and_search_legacy(request: AcemcpRequest) -> Result<CallToolResult, McpError> {
        log_important!(info,
            "Acemcp索引+搜索请求（兼容模式）: project_root_path={}, query={}",
            request.project_root_path, request.query
        );

        // 读取配置
        let mut acemcp_config = Self::get_acemcp_config()
            .await
            .map_err(|e| McpError::internal_error(format!("获取acemcp配置失败: {}", e), None))?;

        // 规范化 base_url（缺协议时补 http://），并去除末尾斜杠
        if let Some(base) = &acemcp_config.base_url {
            let normalized = normalize_base_url(base);
            acemcp_config.base_url = Some(normalized);
        }

        // 先执行索引更新
        match update_index(&acemcp_config, &request.project_root_path).await {
            Ok(_blob_names) => {
                // 索引成功后执行搜索
                match search_only(&acemcp_config, &request.project_root_path, &request.query).await {
                    Ok(text) => Ok(CallToolResult { content: vec![Content::text(text)], is_error: None }),
                    Err(e) => Ok(CallToolResult { content: vec![Content::text(format!("搜索失败: {}", e))], is_error: Some(true) })
                }
            }
            Err(e) => Ok(CallToolResult { content: vec![Content::text(format!("索引更新失败: {}", e))], is_error: Some(true) })
        }
    }

    /// 手动触发索引更新（供 Tauri 命令调用）
    pub async fn trigger_index_update(project_root_path: String) -> Result<String> {
        log_important!(info, "手动触发索引更新: project_root_path={}", project_root_path);

        let acemcp_config = Self::get_acemcp_config().await?;

        match update_index(&acemcp_config, &project_root_path).await {
            Ok(blob_names) => {
                Ok(format!("索引更新成功，共 {} 个 blobs", blob_names.len()))
            }
            Err(e) => {
                Err(anyhow::anyhow!("索引更新失败: {}", e))
            }
        }
    }

    /// 获取项目索引状态（供 Tauri 命令调用）
    pub fn get_index_status(project_root_path: String) -> ProjectIndexStatus {
        get_project_status(&project_root_path)
    }

    /// 获取所有项目的索引状态（供 Tauri 命令调用）
    pub fn get_all_index_status() -> ProjectsIndexStatus {
        load_projects_status()
    }

    /// 获取acemcp配置
    async fn get_acemcp_config() -> Result<AcemcpConfig> {
        // 从配置文件中读取acemcp配置
        let config = crate::config::load_standalone_config()
            .map_err(|e| anyhow::anyhow!("读取配置文件失败: {}", e))?;

        Ok(AcemcpConfig {
            base_url: config.mcp_config.acemcp_base_url,
            token: config.mcp_config.acemcp_token,
            batch_size: config.mcp_config.acemcp_batch_size,
            max_lines_per_blob: config.mcp_config.acemcp_max_lines_per_blob,
            text_extensions: config.mcp_config.acemcp_text_extensions,
            exclude_patterns: config.mcp_config.acemcp_exclude_patterns,
            // 智能等待默认值：1-5 秒随机等待
            smart_wait_range: Some((1, 5)),
        })
    }

    /// 获取工具定义
    pub fn get_tool_definition() -> Tool {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "project_root_path": {
                    "type": "string",
                    "description": "项目根目录的绝对路径，使用正斜杠(/)作为分隔符。例如：C:/Users/username/projects/myproject"
                },
                "query": {
                    "type": "string",
                    "description": "用于查找相关代码上下文的自然语言搜索查询。此工具执行语义搜索并返回与查询匹配的代码片段。例如：'日志配置设置初始化logger'（查找日志设置代码）、'用户认证登录'（查找认证相关代码）、'数据库连接池'（查找数据库连接代码）、'错误处理异常'（查找错误处理模式）、'API端点路由'（查找API路由定义）。工具返回带有文件路径和行号的格式化文本片段，显示相关代码的位置。"
                }
            },
            "required": ["project_root_path", "query"]
        });

        if let serde_json::Value::Object(schema_map) = schema {
            Tool {
                name: Cow::Borrowed("sou"),
                description: Some(Cow::Borrowed("基于查询在特定项目中搜索相关的代码上下文。依赖后台增量索引与文件监听机制维护索引，并在索引进行中通过智能等待在实时性和响应速度之间做平衡。返回代码库中与查询语义相关的格式化文本片段。")),
                input_schema: Arc::new(schema_map),
                annotations: None,
            }
        } else {
            panic!("Schema creation failed");
        }
    }
}

// ---------------- 已移除 Python Web 服务依赖，完全使用 Rust 实现 ----------------

// ---------------- 索引初始化状态枚举 ----------------

/// 索引初始化状态
#[derive(Debug, Clone, PartialEq)]
pub enum InitialIndexState {
    /// 项目记录不存在
    Missing,
    /// 从未索引过（状态为 Idle 且 total_files == 0）
    Idle,
    /// 已完成索引
    Synced,
    /// 正在索引中
    Indexing,
    /// 上次索引失败
    Failed,
}

/// 获取项目的初始索引状态
pub fn get_initial_index_state(project_root: &str) -> InitialIndexState {
    let status = get_project_status(project_root);

    match status.status {
        IndexStatus::Idle if status.total_files == 0 => InitialIndexState::Idle,
        IndexStatus::Idle => InitialIndexState::Missing,
        IndexStatus::Synced => InitialIndexState::Synced,
        IndexStatus::Indexing => InitialIndexState::Indexing,
        IndexStatus::Failed => InitialIndexState::Failed,
    }
}

/// 确保后台索引已启动（非阻塞）
/// 仅在项目未初始化或索引失败时启动后台索引任务
pub async fn ensure_initial_index_background(config: &AcemcpConfig, project_root: &str) -> anyhow::Result<()> {
    let state = get_initial_index_state(project_root);

    match state {
        InitialIndexState::Missing | InitialIndexState::Idle | InitialIndexState::Failed => {
            // 在后台启动索引任务
            let config_clone = config.clone();
            let project_root_clone = project_root.to_string();

            tokio::spawn(async move {
                log_important!(info, "后台索引任务启动: project_root={}", project_root_clone);
                if let Err(e) = update_index(&config_clone, &project_root_clone).await {
                    log_important!(info, "后台索引失败: project_root={}, error={}", project_root_clone, e);
                } else {
                    log_important!(info, "后台索引成功: project_root={}", project_root_clone);
                }
            });

            Ok(())
        }
        InitialIndexState::Synced | InitialIndexState::Indexing => {
            // 已经完成或正在进行，无需操作
            Ok(())
        }
    }
}

// ---------------- 整合 temp 逻辑：索引、上传、检索 ----------------

#[derive(Serialize, Deserialize, Clone)]
struct BlobItem {
    path: String,
    content: String,
}

#[derive(Serialize, Deserialize, Default)]
struct ProjectsFile(HashMap<String, Vec<String>>);

fn normalize_base_url(input: &str) -> String {
    let mut url = input.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        url = format!("http://{}", url);
    }
    while url.ends_with('/') { url.pop(); }
    url
}

async fn retry_request<F, Fut, T>(mut f: F, max_retries: usize, base_delay_secs: f64) -> anyhow::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    let mut attempt = 0usize;
    let mut last_error_str: Option<String> = None;
    
    while attempt < max_retries {
        match f().await {
            Ok(v) => {
                if attempt > 0 {
                    log_debug!("请求在第{}次尝试后成功", attempt + 1);
                }
                return Ok(v);
            }
            Err(e) => {
                last_error_str = Some(e.to_string());
                attempt += 1;
                
                // 检查是否为可重试的错误
                let error_str = e.to_string();
                let is_retryable = error_str.contains("timeout") 
                    || error_str.contains("connection") 
                    || error_str.contains("network")
                    || error_str.contains("temporary");
                
                if attempt >= max_retries || !is_retryable {
                    log_debug!("请求失败，不再重试: {}", e);
                    return Err(e);
                }
                
                let delay = base_delay_secs * 2f64.powi((attempt as i32) - 1);
                let ms = (delay * 1000.0) as u64;
                log_debug!("请求失败，准备重试({}/{}), 等待 {}ms: {}", attempt, max_retries, ms, e);
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
    }
    
    Err(last_error_str
        .and_then(|s| anyhow::anyhow!(s).into())
        .unwrap_or_else(|| anyhow::anyhow!("未知错误")))
}

fn home_projects_file() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let data_dir = home.join(".acemcp").join("data");
    let _ = fs::create_dir_all(&data_dir);
    data_dir.join("projects.json")
}

/// 获取项目索引状态文件路径
fn home_projects_status_file() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let data_dir = home.join(".acemcp").join("data");
    let _ = fs::create_dir_all(&data_dir);
    data_dir.join("projects_status.json")
}

/// 读取所有项目的索引状态
fn load_projects_status() -> ProjectsIndexStatus {
    let status_path = home_projects_status_file();
    if status_path.exists() {
        let data = fs::read_to_string(&status_path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        ProjectsIndexStatus::default()
    }
}

/// 保存所有项目的索引状态
fn save_projects_status(status: &ProjectsIndexStatus) -> Result<()> {
    let status_path = home_projects_status_file();
    let data = serde_json::to_string_pretty(status)?;
    fs::write(status_path, data)?;
    Ok(())
}

/// 更新指定项目的索引状态
fn update_project_status<F>(project_root: &str, updater: F) -> Result<()>
where
    F: FnOnce(&mut ProjectIndexStatus),
{
    let mut all_status = load_projects_status();
    let normalized_root = PathBuf::from(project_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root))
        .to_string_lossy()
        .replace('\\', "/");

    let project_status = all_status.projects
        .entry(normalized_root.clone())
        .or_insert_with(|| {
            let mut status = ProjectIndexStatus::default();
            status.project_root = normalized_root;
            status
        });

    updater(project_status);
    save_projects_status(&all_status)?;
    Ok(())
}

/// 获取指定项目的索引状态
fn get_project_status(project_root: &str) -> ProjectIndexStatus {
    let all_status = load_projects_status();
    let normalized_root = PathBuf::from(project_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root))
        .to_string_lossy()
        .replace('\\', "/");

    all_status.projects.get(&normalized_root).cloned().unwrap_or_else(|| {
        let mut status = ProjectIndexStatus::default();
        status.project_root = normalized_root;
        status
    })
}

/// 读取文件内容，支持多种编码检测
/// 尝试的编码顺序：utf-8, gbk (包含 gb2312), windows-1252 (包含 latin-1)
/// 如果都失败，则使用 utf-8 with errors='ignore'
fn read_file_with_encoding(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return None;
    }

    // 尝试 utf-8
    let (decoded, _, had_errors) = UTF_8.decode(&buf);
    if !had_errors {
        return Some(decoded.into_owned());
    }

    // 尝试 gbk
    let (decoded, _, had_errors) = GBK.decode(&buf);
    if !had_errors {
        log_debug!("成功使用 GBK 编码读取文件: {:?}", path);
        return Some(decoded.into_owned());
    }

    // 尝试 gb2312 (GBK 是 GB2312 的超集，可以处理 GB2312 编码)
    // encoding_rs 中没有单独的 GB2312，使用 GBK 代替
    // GBK 已经在上一步尝试过了，这里跳过

    // 尝试 latin-1 (WINDOWS_1252 是 ISO-8859-1 的超集，可以处理大部分 latin-1 编码)
    let (decoded, _, had_errors) = WINDOWS_1252.decode(&buf);
    if !had_errors {
        log_debug!("成功使用 WINDOWS_1252 编码读取文件: {:?}", path);
        return Some(decoded.into_owned());
    }

    // 如果所有编码都失败，使用 utf-8 with errors='ignore' (lossy 解码)
    let (decoded, _, _) = UTF_8.decode(&buf);
    log_debug!("使用 UTF-8 (lossy) 读取文件，部分字符可能丢失: {:?}", path);
    Some(decoded.into_owned())
}

fn sha256_hex(path: &str, content: &str) -> String {
    let mut ctx = ShaContext::new(&SHA256);
    // 先更新路径的哈希，再更新内容的哈希，与Python版本保持一致
    ctx.update(path.as_bytes());
    ctx.update(content.as_bytes());
    let digest = ctx.finish();
    hex::encode(digest.as_ref())
}

/// 分割文件内容为多个 blob（如果超过最大行数）
/// 与 Python 版本保持一致：chunk 索引从 1 开始
fn split_content(path: &str, content: &str, max_lines: usize) -> Vec<BlobItem> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total_lines = lines.len();
    
    // 如果文件在限制内，返回单个 blob
    if total_lines <= max_lines {
        return vec![BlobItem { path: path.to_string(), content: content.to_string() }];
    }

    // 计算需要的 chunk 数量
    let num_chunks = (total_lines + max_lines - 1) / max_lines;
    let mut blobs = Vec::new();

    // 按 chunk 索引分割（从 0 开始，但显示时从 1 开始）
    for chunk_idx in 0..num_chunks {
        let start_line = chunk_idx * max_lines;
        let end_line = usize::min(start_line + max_lines, total_lines);
        let chunk_lines = &lines[start_line..end_line];
        let chunk_content = chunk_lines.join("");

        // chunk 编号从 1 开始（与 Python 版本保持一致）
        let chunk_path = format!("{}#chunk{}of{}", path, chunk_idx + 1, num_chunks);
        blobs.push(BlobItem { path: chunk_path, content: chunk_content });
    }

    blobs
}

/// 构建排除模式的 GlobSet
fn build_exclude_globset(exclude_patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in exclude_patterns {
        // 尝试将模式转换为 Glob
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        } else {
            log_debug!("无效的排除模式，跳过: {}", pattern);
        }
    }
    builder.build().map_err(|e| anyhow::anyhow!("构建排除模式失败: {}", e))
}

/// 检查路径是否应该被排除
/// 使用 globset 进行完整的 fnmatch 模式匹配（与 Python 版本保持一致）
/// Python 版本使用 fnmatch.fnmatch 检查路径的各个部分和完整路径
fn should_exclude(path: &Path, root: &Path, exclude_globset: Option<&GlobSet>) -> bool {
    if exclude_globset.is_none() {
        return false;
    }
    let globset = exclude_globset.unwrap();

    // 获取相对路径
    let rel = match path.strip_prefix(root) {
        Ok(rel) => rel,
        Err(_) => path,
    };

    // 转换为使用正斜杠的字符串（用于匹配）
    let rel_forward = rel.to_string_lossy().replace('\\', "/");
    
    // 检查完整相对路径（与 Python 版本的 fnmatch(path_str, pattern) 一致）
    if globset.is_match(&rel_forward) {
        return true;
    }

    // 检查路径的各个部分（与 Python 版本的 fnmatch(part, pattern) 一致）
    for part in rel.iter() {
        if let Some(part_str) = part.to_str() {
            if globset.is_match(part_str) {
                return true;
            }
        }
    }

    false
}

fn build_gitignore(root: &Path) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    let gi_path = root.join(".gitignore");
    if gi_path.exists() {
        if builder.add(gi_path).is_some() { return None; }
        return match builder.build() { Ok(gi) => Some(gi), Err(_) => None };
    }
    None
}

fn collect_blobs(root: &str, text_exts: &[String], exclude_patterns: &[String], max_lines_per_blob: usize) -> anyhow::Result<Vec<BlobItem>> {
    let root_path = PathBuf::from(root);
    if !root_path.exists() { anyhow::bail!("项目根目录不存在: {}", root); }
    
    log_important!(info, "开始收集代码文件: 根目录={}, 扩展名={:?}, 排除模式={:?}", root, text_exts, exclude_patterns);
    
    // 构建排除模式的 GlobSet
    let exclude_globset = if exclude_patterns.is_empty() {
        None
    } else {
        match build_exclude_globset(exclude_patterns) {
            Ok(gs) => Some(gs),
            Err(e) => {
                log_debug!("构建排除模式失败，将使用简单匹配: {}", e);
                None
            }
        }
    };
    
    let mut out = Vec::new();
    let gitignore = build_gitignore(&root_path);
    let mut dirs_stack = vec![root_path.clone()];
    let mut scanned_files = 0;
    let mut indexed_files = 0;
    let mut excluded_count = 0;
    
    while let Some(dir) = dirs_stack.pop() {
        let entries = match fs::read_dir(&dir) { Ok(e) => e, Err(_) => continue };
        for entry in entries.flatten() {
            let p = entry.path();
            
            // 检查 .gitignore
            if let Some(gi) = &gitignore {
                if gi.matched_path_or_any_parents(&p, p.is_dir()).is_ignore() { continue; }
            }
            
            // 检查排除模式
            if p.is_dir() {
                if should_exclude(&p, &root_path, exclude_globset.as_ref()) {
                    excluded_count += 1;
                    continue;
                }
                dirs_stack.push(p);
                continue;
            }
            
            scanned_files += 1;
            if should_exclude(&p, &root_path, exclude_globset.as_ref()) {
                excluded_count += 1;
                log_debug!("排除文件: {:?}", p);
                continue;
            }
            
            // 检查文件扩展名
            let ext_ok = p.extension().and_then(|s| s.to_str()).map(|e| {
                let dot = format!(".{}", e).to_lowercase();
                text_exts.iter().any(|te| te.eq_ignore_ascii_case(&dot))
            }).unwrap_or(false);
            if !ext_ok { continue; }
            
            // 读取文件内容（使用多编码支持）
            let rel = p.strip_prefix(&root_path).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if let Some(content) = read_file_with_encoding(&p) {
                let parts = split_content(&rel, &content, max_lines_per_blob);
                let blob_count = parts.len();
                indexed_files += 1;
                out.extend(parts);
                log_important!(info, "索引文件: path={}, content_length={}, blobs={}", rel, content.len(), blob_count);
            } else {
                log_debug!("无法读取文件: {:?}", p);
            }
        }
    }
    
    log_important!(info, "文件收集完成: 扫描文件数={}, 索引文件数={}, 生成blobs数={}, 排除文件/目录数={}", scanned_files, indexed_files, out.len(), excluded_count);
    Ok(out)
}

/// 只执行索引更新，不进行搜索
/// 返回值：成功上传的 blob 名称列表
pub(crate) async fn update_index(config: &AcemcpConfig, project_root_path: &str) -> anyhow::Result<Vec<String>> {
    let base_url = config.base_url.clone().ok_or_else(|| anyhow::anyhow!("未配置 base_url"))?;
    // 严格校验 base_url
    let has_scheme = base_url.starts_with("http://") || base_url.starts_with("https://");
    let has_host = base_url.trim().len() > "https://".len();
    if !has_scheme || !has_host { anyhow::bail!("无效的 base_url，请填写完整的 http(s)://host[:port] 格式"); }
    let token = config.token.clone().ok_or_else(|| anyhow::anyhow!("未配置 token"))?;
    let batch_size = config.batch_size.unwrap_or(10) as usize;
    let max_lines = config.max_lines_per_blob.unwrap_or(800) as usize;
    let text_exts = config.text_extensions.clone().unwrap_or_default();
    let exclude_patterns = config.exclude_patterns.clone().unwrap_or_default();

    // 更新状态：开始索引
    let _ = update_project_status(project_root_path, |status| {
        status.status = IndexStatus::Indexing;
        status.progress = 0;
    });

    // 日志：基础配置
    log_important!(info,
        "=== 开始索引代码库 ==="
    );
    log_important!(info,
        "Acemcp配置: base_url={}, batch_size={}, max_lines_per_blob={}, text_exts数量={}, exclude_patterns数量={}",
        base_url,
        batch_size,
        max_lines,
        text_exts.len(),
        exclude_patterns.len()
    );
    log_important!(info,
        "项目路径: {}", project_root_path
    );

    // 收集 blob（根据扩展名与排除规则，简化版 .gitignore 支持）
    log_important!(info, "开始收集代码文件...");
    let blobs = collect_blobs(project_root_path, &text_exts, &exclude_patterns, max_lines)?;
    if blobs.is_empty() {
        // 更新状态：失败
        let _ = update_project_status(project_root_path, |status| {
            status.status = IndexStatus::Failed;
            status.last_error = Some("未在项目中找到可索引的文本文件".to_string());
            status.last_failure_time = Some(chrono::Utc::now());
        });
        anyhow::bail!("未在项目中找到可索引的文本文件");
    }

    // 更新状态：文件收集完成
    let _ = update_project_status(project_root_path, |status| {
        status.total_files = blobs.len();
        status.progress = 20;
    });

    // 加载 projects.json
    let projects_path = home_projects_file();
    let mut projects: ProjectsFile = if projects_path.exists() {
        let data = fs::read_to_string(&projects_path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else { ProjectsFile::default() };

    let normalized_root = PathBuf::from(project_root_path).canonicalize().unwrap_or_else(|_| PathBuf::from(project_root_path)).to_string_lossy().replace('\\', "/");
    let existing_blob_names: std::collections::HashSet<String> = projects.0.get(&normalized_root).cloned().unwrap_or_default().into_iter().collect();

    // 计算所有 blob 的哈希值，建立哈希到 blob 的映射
    let mut blob_hash_map: std::collections::HashMap<String, BlobItem> = std::collections::HashMap::new();
    for blob in &blobs {
        let hash = sha256_hex(&blob.path, &blob.content);
        blob_hash_map.insert(hash.clone(), blob.clone());
    }

    // 分离已存在和新增加的 blob（与 Python 版本保持一致）
    let all_blob_hashes: std::collections::HashSet<String> = blob_hash_map.keys().cloned().collect();
    let existing_hashes: std::collections::HashSet<String> = all_blob_hashes.intersection(&existing_blob_names).cloned().collect();
    let new_hashes: std::collections::HashSet<String> = all_blob_hashes.difference(&existing_blob_names).cloned().collect();

    // 需要上传的新 blob
    let new_blobs: Vec<BlobItem> = new_hashes.iter().filter_map(|h| blob_hash_map.get(h).cloned()).collect();

    log_important!(info,
        "=== 索引统计 ==="
    );
    log_important!(info,
        "收集到blobs总数: {}, 既有blobs: {}, 新增blobs: {}, 需要上传: {}",
        blobs.len(),
        existing_hashes.len(),
        new_hashes.len(),
        new_blobs.len()
    );

    let client = Client::new();

    // 批量上传新增 blobs
    let mut uploaded_names: Vec<String> = Vec::new();
    let mut failed_batches: Vec<usize> = Vec::new();
    
    if !new_blobs.is_empty() {
        let total_batches = (new_blobs.len() + batch_size - 1) / batch_size;
        log_important!(info,
            "=== 开始批量上传代码索引 ==="
        );
        log_important!(info,
            "目标端点: {}/batch-upload, 总批次: {}, 每批上限: {}, 总blobs: {}",
            base_url,
            total_batches,
            batch_size,
            new_blobs.len()
        );
        
        for i in 0..total_batches {
            let start = i * batch_size;
            let end = usize::min(start + batch_size, new_blobs.len());
            let batch = &new_blobs[start..end];
            let url = format!("{}/batch-upload", base_url);
            
            log_important!(info,
                "上传批次 {}/{}: url={}, blobs={}",
                i + 1,
                total_batches,
                url,
                batch.len()
            );
            
            // 详细记录每个 blob 的信息
            for (idx, blob) in batch.iter().enumerate() {
                log_important!(info,
                    "  批次 {} - Blob {}/{}: path={}, content_length={}",
                    i + 1,
                    idx + 1,
                    batch.len(),
                    blob.path,
                    blob.content.len()
                );
            }
            
            let payload = serde_json::json!({"blobs": batch});
            log_important!(info, "批次载荷大小: {} 字节", payload.to_string().len());
            
            match retry_request(|| async {
                let r = client
                    .post(&url)
                    .header(AUTHORIZATION, format!("Bearer {}", token))
                    .header(CONTENT_TYPE, "application/json")
                    .json(&payload)
                    .send()
                    .await?;
                
                let status = r.status();
                log_important!(info, "HTTP响应状态: {}", status);
                
                if !status.is_success() {
                    let body = r.text().await.unwrap_or_default();
                    anyhow::bail!("HTTP {} {}", status, body);
                }
                
                let v: serde_json::Value = r.json().await?;
                log_important!(info, "响应数据: {}", serde_json::to_string_pretty(&v).unwrap_or_default());
                Ok(v)
            }, 3, 1.0).await {
                Ok(value) => {
                    if let Some(arr) = value.get("blob_names").and_then(|v| v.as_array()) {
                        let mut batch_names: Vec<String> = Vec::new();
                        for v in arr { 
                            if let Some(s) = v.as_str() { 
                                batch_names.push(s.to_string()); 
                            }
                        }
                        
                        if batch_names.is_empty() {
                            log_important!(info, "批次 {} 返回了空的blob名称列表", i + 1);
                            failed_batches.push(i + 1);
                        } else {
                            uploaded_names.extend(batch_names.clone());
                            log_important!(info, "批次 {} 上传成功，获得 {} 个blob名称", i + 1, batch_names.len());
                            // 详细记录每个上传成功的 blob 名称
                            for (idx, name) in batch_names.iter().enumerate() {
                                log_important!(info, "  批次 {} - 上传成功 Blob {}/{}: name={}", i + 1, idx + 1, batch_names.len(), name);
                            }
                        }
                    } else {
                        log_important!(info, "批次 {} 响应中缺少blob_names字段", i + 1);
                        failed_batches.push(i + 1);
                    }
                }
                Err(e) => {
                    log_important!(info, "批次 {} 上传失败: {}", i + 1, e);
                    failed_batches.push(i + 1);
                }
            }
        }
        
        // 上传结果总结
        log_important!(info,
            "=== 上传结果总结 ==="
        );
        if !failed_batches.is_empty() {
            log_important!(info, "上传完成，但有失败的批次: {:?}, 成功上传blobs: {}", failed_batches, uploaded_names.len());
        } else {
            log_important!(info, "所有批次上传成功，共上传 {} 个blobs", uploaded_names.len());
        }
    } else {
        log_important!(info, "没有新的blob需要上传，使用已有索引");
    }

    // 合并并保存 projects.json（与 Python 版本保持一致）
    // 只保留当前项目中仍然存在的 blob 的哈希值（自动删除已删除的 blob）
    let all_blob_names: Vec<String> = existing_hashes.into_iter().chain(uploaded_names.into_iter()).collect();
    projects.0.insert(normalized_root.clone(), all_blob_names.clone());
    if let Ok(s) = serde_json::to_string_pretty(&projects) { let _ = fs::write(projects_path, s); }

    // 使用合并后的 blob_names（与 Python 版本保持一致）
    let blob_names = all_blob_names;
    if blob_names.is_empty() {
        log_important!(info, "索引后未找到 blobs，项目路径: {}", normalized_root);
        // 更新状态：失败
        let _ = update_project_status(project_root_path, |status| {
            status.status = IndexStatus::Failed;
            status.last_error = Some("索引后未找到 blobs".to_string());
            status.last_failure_time = Some(chrono::Utc::now());
        });
        anyhow::bail!("索引后未找到 blobs");
    }

    // 检查是否是首次成功索引（用于 ji 集成）
    let is_first_success = {
        let status = get_project_status(project_root_path);
        status.last_success_time.is_none()
    };

    // 更新状态：索引成功完成
    let _ = update_project_status(project_root_path, |status| {
        status.status = IndexStatus::Synced;
        status.progress = 100;
        status.indexed_files = blobs.len();
        status.pending_files = 0;
        status.last_success_time = Some(chrono::Utc::now());
        status.last_error = None;
    });

    // 首次成功索引时，写入 ji 记忆
    if is_first_success {
        let _ = write_index_memory_to_ji(project_root_path, config);
    }

    log_important!(info, "索引更新完成，共 {} 个 blobs", blob_names.len());
    Ok(blob_names)
}

/// 将索引配置信息写入 ji（记忆）工具
fn write_index_memory_to_ji(project_root_path: &str, config: &AcemcpConfig) {
    use super::super::memory::MemoryManager;
    use super::super::memory::MemoryCategory;

    // 创建记忆管理器
    let manager = match MemoryManager::new(project_root_path) {
        Ok(m) => m,
        Err(e) => {
            log_debug!("创建记忆管理器失败（不影响索引）: {}", e);
            return;
        }
    };

    // 构建记忆内容
    let text_exts = config.text_extensions.clone().unwrap_or_default();
    let exclude_patterns = config.exclude_patterns.clone().unwrap_or_default();
    let batch_size = config.batch_size.unwrap_or(10);
    let max_lines = config.max_lines_per_blob.unwrap_or(800);

    let memory_content = format!(
        "acemcp 代码索引已启用 - 配置摘要: 文件扩展名={:?}, 排除模式={:?}, 批次大小={}, 最大行数/块={}",
        text_exts, exclude_patterns, batch_size, max_lines
    );

    // 写入记忆
    match manager.add_memory(&memory_content, MemoryCategory::Context) {
        Ok(id) => {
            log_important!(info, "已将索引配置写入 ji 记忆: id={}", id);
        }
        Err(e) => {
            log_debug!("写入 ji 记忆失败（不影响索引）: {}", e);
        }
    }
}

/// 只执行搜索，不触发索引
/// 使用已有的索引数据进行搜索
async fn search_only(config: &AcemcpConfig, project_root_path: &str, query: &str) -> anyhow::Result<String> {
    let base_url = config.base_url.clone().ok_or_else(|| anyhow::anyhow!("未配置 base_url"))?;
    let token = config.token.clone().ok_or_else(|| anyhow::anyhow!("未配置 token"))?;

    // 从 projects.json 读取已有的 blob 名称
    let projects_path = home_projects_file();
    let projects: ProjectsFile = if projects_path.exists() {
        let data = fs::read_to_string(&projects_path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        ProjectsFile::default()
    };

    let normalized_root = PathBuf::from(project_root_path)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(project_root_path))
        .to_string_lossy()
        .replace('\\', "/");

    let blob_names = projects.0.get(&normalized_root).cloned().unwrap_or_default();

    if blob_names.is_empty() {
        anyhow::bail!("项目尚未索引或索引为空，请先执行索引操作");
    }

    // 发起检索
    log_important!(info,
        "=== 开始代码检索（仅搜索模式） ==="
    );
    let search_url = format!("{}/agents/codebase-retrieval", base_url);
    log_important!(info, "检索请求: url={}, 使用blobs数量={}, 查询内容={}", search_url, blob_names.len(), query);

    let payload = serde_json::json!({
        "information_request": query,
        "blobs": {"checkpoint_id": serde_json::Value::Null, "added_blobs": blob_names, "deleted_blobs": []},
        "dialog": [],
        "max_output_length": 0,
        "disable_codebase_retrieval": false,
        "enable_commit_retrieval": false,
    });

    log_important!(info, "检索载荷大小: {} 字节", payload.to_string().len());

    let client = Client::new();
    let value: serde_json::Value = retry_request(|| async {
        let r = client
            .post(&search_url)
            .header(AUTHORIZATION, format!("Bearer {}", token))
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = r.status();
        log_important!(info, "检索请求HTTP响应状态: {}", status);

        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {} {}", status, body);
        }

        let v: serde_json::Value = r.json().await?;
        log_important!(info, "检索响应数据: {}", serde_json::to_string_pretty(&v).unwrap_or_default());
        Ok(v)
    }, 3, 2.0).await?;

    let text = value
        .get("formatted_retrieval")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if text.is_empty() {
        log_important!(info, "搜索返回空结果");
        Ok("No relevant code context found for your query.".to_string())
    } else {
        log_important!(info, "搜索成功，返回文本长度: {}", text.len());
        Ok(text)
    }
}
