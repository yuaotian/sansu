use anyhow::Result;
use rmcp::model::{ErrorData as McpError, Content};

use crate::mcp::types::{McpResponse, McpResponseContent};

/// 解析 MCP 响应内容
///
/// 支持新的结构化格式和旧格式的兼容性，并生成适当的 Content 对象
pub fn parse_mcp_response(response: &str) -> Result<Vec<Content>, McpError> {
    if response.trim() == "CANCELLED" || response.trim() == "用户取消了操作" {
        return Ok(vec![Content::text("用户取消了操作".to_string())]);
    }

    // 首先尝试解析为新的结构化格式
    if let Ok(structured_response) = serde_json::from_str::<McpResponse>(response) {
        return parse_structured_response(structured_response);
    }

    // 回退到旧格式兼容性解析
    match serde_json::from_str::<Vec<McpResponseContent>>(response) {
        Ok(content_array) => {
            let mut result = Vec::new();
            let mut image_count = 0;

            // 分别收集用户文本和图片信息
            let mut user_text_parts = Vec::new();
            let mut image_info_parts = Vec::new();

            for content in content_array {
                match content.content_type.as_str() {
                    "text" => {
                        if let Some(text) = content.text {
                            user_text_parts.push(text);
                        }
                    }
                    "image" => {
                        if let Some(source) = content.source {
                            if source.source_type == "base64" {
                                image_count += 1;

                                // 先添加图片到结果中（图片在前）
                                result.push(Content::image(source.data.clone(), source.media_type.clone()));

                                // 添加图片信息到图片信息部分
                                let base64_len = source.data.len();
                                let preview = if base64_len > 50 {
                                    format!("{}...", &source.data[..50])
                                } else {
                                    source.data.clone()
                                };

                                // 计算图片大小（base64解码后的大小）
                                let estimated_size = (base64_len * 3) / 4; // base64编码后大约增加33%
                                let size_str = if estimated_size < 1024 {
                                    format!("{} B", estimated_size)
                                } else if estimated_size < 1024 * 1024 {
                                    format!("{:.1} KB", estimated_size as f64 / 1024.0)
                                } else {
                                    format!("{:.1} MB", estimated_size as f64 / (1024.0 * 1024.0))
                                };

                                let image_info = format!(
                                    "=== 图片 {} ===\n类型: {}\n大小: {}\nBase64 预览: {}\n完整 Base64 长度: {} 字符",
                                    image_count, source.media_type, size_str, preview, base64_len
                                );
                                image_info_parts.push(image_info);
                            }
                        }
                    }
                    _ => {
                        // 未知类型，作为文本处理
                        if let Some(text) = content.text {
                            user_text_parts.push(text);
                        }
                    }
                }
            }

            // 构建文本内容：用户文本 + 图片信息 + 注意事项
            let mut all_text_parts = Vec::new();

            // 1. 用户输入的文本
            if !user_text_parts.is_empty() {
                all_text_parts.extend(user_text_parts);
            }

            // 2. 图片详细信息
            if !image_info_parts.is_empty() {
                all_text_parts.extend(image_info_parts);
            }

            // 3. 兼容性说明
            if image_count > 0 {
                all_text_parts.push(format!(
                    "💡 注意：用户提供了 {} 张图片。如果 AI 助手无法显示图片，图片数据已包含在上述 Base64 信息中。",
                    image_count
                ));
            }

            // 将所有文本内容合并并添加到结果末尾（图片后面）
            if !all_text_parts.is_empty() {
                let combined_text = all_text_parts.join("\n\n");
                result.push(Content::text(combined_text));
            }

            if result.is_empty() {
                result.push(Content::text("用户未提供任何内容".to_string()));
            }

            Ok(result)
        }
        Err(_) => {
            // 如果不是JSON格式，作为纯文本处理
            Ok(vec![Content::text(response.to_string())])
        }
    }
}

/// 解析新的结构化响应格式
fn parse_structured_response(response: McpResponse) -> Result<Vec<Content>, McpError> {
    let mut result = Vec::new();
    let mut text_parts = Vec::new();

    // 1. 处理选择的选项
    if !response.selected_options.is_empty() {
        text_parts.push(format!("选择的选项: {}", response.selected_options.join(", ")));
    }

    // 2. 处理用户输入文本
    if let Some(user_input) = response.user_input {
        if !user_input.trim().is_empty() {
            text_parts.push(user_input.trim().to_string());
        }
    }

    // 3. 处理图片附件
    let mut image_info_parts = Vec::new();
    for (index, image) in response.images.iter().enumerate() {
        // 添加图片到结果中（图片在前）
        result.push(Content::image(image.data.clone(), image.media_type.clone()));

        // 生成图片信息
        let base64_len = image.data.len();
        let preview = if base64_len > 50 {
            format!("{}...", &image.data[..50])
        } else {
            image.data.clone()
        };

        // 计算图片大小
        let estimated_size = (base64_len * 3) / 4;
        let size_str = if estimated_size < 1024 {
            format!("{} B", estimated_size)
        } else if estimated_size < 1024 * 1024 {
            format!("{:.1} KB", estimated_size as f64 / 1024.0)
        } else {
            format!("{:.1} MB", estimated_size as f64 / (1024.0 * 1024.0))
        };

        let filename_info = image.filename.as_ref()
            .map(|f| format!("\n文件名: {}", f))
            .unwrap_or_default();

        let image_info = format!(
            "=== 图片 {} ==={}\n类型: {}\n大小: {}\nBase64 预览: {}\n完整 Base64 长度: {} 字符",
            index + 1, filename_info, image.media_type, size_str, preview, base64_len
        );
        image_info_parts.push(image_info);
    }

    // 4. 合并所有文本内容
    let mut all_text_parts = text_parts;
    all_text_parts.extend(image_info_parts);

    // 5. 添加兼容性说明
    if !response.images.is_empty() {
        all_text_parts.push(format!(
            "💡 注意：用户提供了 {} 张图片。如果 AI 助手无法显示图片，图片数据已包含在上述 Base64 信息中。",
            response.images.len()
        ));
    }

    // 6. 将文本内容添加到结果中（图片后面）
    if !all_text_parts.is_empty() {
        let combined_text = all_text_parts.join("\n\n");
        result.push(Content::text(combined_text));
    }

    // 7. 如果没有任何内容，添加默认响应
    if result.is_empty() {
        result.push(Content::text("用户未提供任何内容".to_string()));
    }

    Ok(result)
}
