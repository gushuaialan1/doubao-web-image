use anyhow::Result;
use clap::Parser;
use doubao_web_image::client::DoubaoClient;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "doubao-web-image")]
#[command(about = "豆包 Web 端自动化生图工具 (Rust + chromiumoxide)")]
#[command(version = "1.4.1")]
struct Args {
    /// 生图提示词
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// 同对话批量生图模式：plan.json 计划文件路径（提供后忽略位置参数 PROMPT）
    #[arg(long, value_name = "PLAN_JSON")]
    batch: Option<PathBuf>,

    /// 显示浏览器窗口（首次登录必须带此参数）
    #[arg(long)]
    ui: bool,

    /// 图片质量: preview 或 original（默认 original）
    #[arg(long, value_name = "QUALITY", default_value = "original")]
    quality: String,

    /// 图片比例（如 16:9, 1:1, 9:16）
    #[arg(long, value_name = "RATIO")]
    ratio: Option<String>,

    /// 输出文件路径
    #[arg(long, value_name = "PATH", default_value = "generated.png")]
    output: PathBuf,

    /// --image 是 --output 的别名
    #[arg(long, value_name = "PATH")]
    image: Option<PathBuf>,

    /// 去除左上角水印（AI 生成标签）
    #[arg(long)]
    no_watermark: bool,

    /// 参考图路径（可重复，最多 4 张；也支持逗号分隔：--reference=a.png,b.png）
    #[arg(long, value_name = "PATH", value_delimiter = ',')]
    reference: Vec<PathBuf>,

    /// 每张图片的等待超时（毫秒）。单图模式默认 120000，批量模式默认 180000
    #[arg(long, value_name = "MS")]
    timeout_ms: Option<u64>,
}

/// 参考图数量上限
const MAX_REFERENCES: usize = 4;

/// 豆包附件输入框接受的图片格式
const REFERENCE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp"];

/// 校验参考图：文件必须存在、扩展名合法、数量不超过上限，返回规范化绝对路径。
fn validate_references(inputs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    if inputs.len() > MAX_REFERENCES {
        anyhow::bail!("参考图最多 {MAX_REFERENCES} 张，实际提供了 {} 张", inputs.len());
    }
    let mut out = Vec::with_capacity(inputs.len());
    for p in inputs {
        if !p.is_file() {
            anyhow::bail!("参考图不存在: {}", p.display());
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if !REFERENCE_EXTS.contains(&ext.as_str()) {
            anyhow::bail!(
                "参考图仅支持 {} 格式: {}",
                REFERENCE_EXTS.join("/"),
                p.display()
            );
        }
        out.push(std::fs::canonicalize(p)?);
    }
    Ok(out)
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("\n❌ 发生致命错误: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    // 同对话批量生图模式：--batch=plan.json 时忽略位置参数 PROMPT
    if let Some(plan_path) = &args.batch {
        return run_batch(plan_path, &args).await;
    }

    // If no prompt provided, show help
    let prompt = match args.prompt {
        Some(p) if !p.trim().is_empty() => p,
        _ => {
            // Print custom help with examples
            println!(
                r#"
豆包 Web 端自动化生图工具 (Rust + chromiumoxide)

用法:
    doubao-web-image.exe "<提示词>" [选项]
    doubao-web-image.exe --batch=plan.json [选项]

选项:
    --ui                    显示浏览器窗口（首次登录必须带此参数）
    --quality=<QUALITY>     图片质量: preview 或 original (默认: original)
    --ratio=<RATIO>         图片比例 (如: 16:9, 1:1, 9:16, 2:3, 3:4, 4:3)
    --output=<PATH>         输出文件路径 (默认: generated.png)
    --image=<PATH>          --output 的别名
    --reference=<PATH>      参考图路径（可重复，最多 4 张；支持逗号分隔）
    --no-watermark          去除左上角水印（AI 生成标签）
    --batch=<PLAN_JSON>     同对话批量生图模式（plan.json 描述 context 与 items）
    --timeout-ms=<MS>       每张图片的等待超时（单图默认 120000，批量默认 180000）
    -h, --help              显示帮助
    -V, --version           显示版本

示例:
    首次使用（需登录）:
        doubao-web-image.exe "一只可爱的猫咪" --ui

    日常生图（无头模式）:
        doubao-web-image.exe "赛博朋克风格的城市夜景"

    指定比例和输出路径:
        doubao-web-image.exe "星空下的赛博朋克城市" --ratio=9:16 --output=./wallpaper.png

    带参考图（保持商品外观一致，如推书场景）:
        doubao-web-image.exe "参考这本书的封面，生成书桌上的展示图" --reference=./book-cover.png

    同对话批量生图（保持人物/风格一致性）:
        doubao-web-image.exe --batch=plan.json --timeout-ms=180000
"#
            );
            return Ok(());
        }
    };

    let output_path = args.image.unwrap_or(args.output);
    let headless = !args.ui;
    let quality = args.quality;
    let ratio = args.ratio.as_deref();
    let no_watermark = args.no_watermark;
    let references = validate_references(&args.reference)?;
    let timeout_ms = args.timeout_ms.unwrap_or(120_000);

    println!("--- 启动豆包生图客户端 ---");

    let mut client = DoubaoClient::new()?;
    let mut needs_ui_retry = false;
    let mut saved_result: Option<(PathBuf, bool)> = None;

    // First attempt
    match try_generate(
        &mut client,
        headless,
        &prompt,
        &quality,
        ratio,
        &output_path,
        &references,
        timeout_ms,
    )
    .await
    {
        Ok((path, is_watermark_free)) => {
            saved_result = Some((path, is_watermark_free));
        }
        Err(e) => {
            if headless {
                println!("\n⚠️ 未能获取到图片: {e}");
                needs_ui_retry = true;
            } else {
                eprintln!("\n❌ 失败: {e}");
            }
        }
    }

    client.close().await;

    // UI retry if headless failed
    if needs_ui_retry && saved_result.is_none() {
        println!("\n=============================================");
        println!("🔄 正在自动以 UI 模式重启...");
        println!("💡 如果出现验证码，请在浏览器中手动完成。");
        println!("=============================================\n");

        let mut client = DoubaoClient::new()?;
        match try_generate(
            &mut client,
            false,
            &prompt,
            &quality,
            ratio,
            &output_path,
            &references,
            timeout_ms,
        )
        .await
        {
            Ok((path, is_watermark_free)) => {
                saved_result = Some((path, is_watermark_free));
            }
            Err(e) => {
                eprintln!("\n❌ UI 模式重试失败: {e}");
            }
        }
        client.close().await;
    }

    if let Some((path, is_watermark_free)) = saved_result {
        println!("\n✅ 成功!");
        println!("💾 图片已保存至: {}", path.display());

        // Apply watermark removal only when requested and the URL is not already
        // the watermark-free original (image_ori_raw) extracted from the SSE stream.
        if no_watermark && !is_watermark_free {
            match remove_watermark(&path) {
                Ok(()) => println!("🧹 水印已去除"),
                Err(e) => eprintln!("⚠️ 水印去除失败: {e}"),
            }
        } else if no_watermark && is_watermark_free {
            println!("🧹 已直接下载无水印原图，无需额外处理");
        }
    } else {
        std::process::exit(1);
    }

    Ok(())
}

fn remove_watermark(path: &PathBuf) -> Result<()> {
    use image::{GenericImageView, ImageReader, imageops};
    use std::io::Cursor;

    println!("[Watermark] 正在去除水印...");

    // Read image
    let img = ImageReader::open(path)?
        .decode()
        .map_err(|e| anyhow::anyhow!("Failed to decode image: {e}"))?;

    let (width, height) = img.dimensions();
    println!("[Watermark] 原图尺寸: {width}x{height}");

    // Calculate crop amount: 8% of shorter side, minimum 60px
    // Watermark analysis shows tag is ~92px tall on 1773px short side (~5.2%)
    // Using 8% with min 60px ensures complete removal across all image sizes
    let shorter_side = width.min(height);
    let crop_px = (shorter_side as f32 * 0.08).max(60.0) as u32;
    println!("[Watermark] 将裁切顶部 {crop_px} 像素区域");

    // Algorithm: scale up proportionally, then crop from top
    // This preserves aspect ratio (no stretching distortion)
    // scale = h / (h - crop_px) so that after cropping we get original dimensions
    let scale = height as f32 / (height - crop_px) as f32;
    let new_width = (width as f32 * scale).ceil() as u32;
    let new_height = (height as f32 * scale).ceil() as u32;
    println!("[Watermark] 等比例放大至 {new_width}x{new_height} (scale={scale:.4})");

    // Scale up the entire image proportionally
    let scaled = imageops::resize(&img, new_width, new_height, imageops::FilterType::Lanczos3);

    // After scaling, watermark occupies top (crop_px * scale) pixels
    // Crop starting from that offset, centered horizontally
    let offset_y = (crop_px as f32 * scale).ceil() as u32;
    let offset_x = (new_width - width) / 2;
    println!("[Watermark] 从 ({offset_x}, {offset_y}) 裁切 {width}x{height}");

    let cropped = imageops::crop_imm(&scaled, offset_x, offset_y, width, height);
    let result = cropped.to_image();

    // Save back
    let mut output_buf = Vec::new();
    let mut cursor = Cursor::new(&mut output_buf);
    result
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| anyhow::anyhow!("Failed to encode image: {e}"))?;

    std::fs::write(path, &output_buf)?;
    println!("[Watermark] 已保存处理后的图片");

    Ok(())
}

async fn try_generate(
    client: &mut DoubaoClient,
    headless: bool,
    prompt: &str,
    quality: &str,
    ratio: Option<&str>,
    output: &PathBuf,
    references: &[PathBuf],
    timeout_ms: u64,
) -> Result<(PathBuf, bool)> {
    client.init(headless).await?;

    println!(
        "\n任务: 生成图片 \"{prompt}\" (质量: {quality}{}{})",
        ratio.map(|r| format!(", 比例: {r}")).unwrap_or_default(),
        if references.is_empty() {
            String::new()
        } else {
            format!(", 参考图: {} 张", references.len())
        }
    );

    let image_info = match client
        .generate_image(prompt, quality, ratio, timeout_ms, references)
        .await
    {
        Ok(Some(info)) => info,
        // 首轮取图失败（超时/取 URL 失败）：在同一对话里补发一次「重新生成」再取
        Ok(None) => {
            println!("\n⚠️ 首轮未等到图片，在同一对话补发一次重新生成...");
            client
                .regenerate_and_wait(quality, timeout_ms)
                .await?
                .ok_or_else(|| anyhow::anyhow!("未能获取图片 URL"))?
        }
        Err(e) => {
            println!("\n⚠️ 首轮取图出错: {e}，在同一对话补发一次重新生成...");
            match client.regenerate_and_wait(quality, timeout_ms).await {
                Ok(Some(info)) => info,
                _ => return Err(e),
            }
        }
    };

    println!("\n✅ 成功!");
    println!(
        "图片链接: {} {}",
        image_info.url,
        if image_info.is_watermark_free {
            "(无水印原图)"
        } else {
            ""
        }
    );

    let saved = client.download_with_page(&image_info.url, output).await?;
    Ok((saved, image_info.is_watermark_free))
}

// ==================== 同对话批量生图模式（--batch） ====================

/// 批量计划文件（plan.json）。
#[derive(Debug, serde::Deserialize)]
struct BatchPlan {
    /// 可选。整篇文案，作为对话首条消息发出（纯文字，不期待图片产出），
    /// 用于给豆包建立全文上下文。
    context: Option<String>,

    /// 图片比例（如 9:16），应用到所有 item
    ratio: Option<String>,

    /// 图片质量：preview 或 original，默认 original
    quality: Option<String>,

    /// 是否去除左上角水印（AI 生成标签）
    #[serde(default, rename = "noWatermark", alias = "no_watermark")]
    no_watermark: bool,

    /// 生图条目，逐个在同一对话中发送
    items: Vec<BatchItem>,
}

#[derive(Debug, serde::Deserialize)]
struct BatchItem {
    /// 完整生图 prompt
    prompt: String,
    /// 输出文件路径（建议绝对路径）
    output: PathBuf,
}

/// 单个 item 的执行结果（序列化进 stdout 的 JSON 摘要）。
#[derive(Debug, serde::Serialize)]
struct BatchResultItem {
    output: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// 同对话批量生图：打开一个豆包对话，先发可选的 context，再逐条发送 items 的
/// prompt 并下载图片。单张失败只记录错误、继续下一张；全部失败才以非零码退出。
async fn run_batch(plan_path: &PathBuf, args: &Args) -> Result<()> {
    // 1. 解析并校验计划文件
    let raw = std::fs::read_to_string(plan_path).map_err(|e| {
        anyhow::anyhow!("无法读取批量计划文件 {}: {e}", plan_path.display())
    })?;
    let plan: BatchPlan = serde_json::from_str(&raw).map_err(|e| {
        anyhow::anyhow!("批量计划文件 JSON 解析失败 {}: {e}", plan_path.display())
    })?;
    if plan.items.is_empty() {
        anyhow::bail!("批量计划 items 为空: {}", plan_path.display());
    }
    for (i, item) in plan.items.iter().enumerate() {
        if item.prompt.trim().is_empty() {
            anyhow::bail!("批量计划第 {} 项 prompt 为空", i);
        }
        if item.output.as_os_str().is_empty() {
            anyhow::bail!("批量计划第 {} 项 output 为空", i);
        }
    }

    let quality = plan.quality.clone().unwrap_or_else(|| "original".to_string());
    let ratio = plan.ratio.clone();
    let no_watermark = plan.no_watermark;
    let timeout_ms = args.timeout_ms.unwrap_or(180_000);
    let headless = !args.ui;
    let total = plan.items.len();

    println!("--- 启动豆包批量生图客户端（同对话模式） ---");
    println!(
        "计划: {} 张图片, 质量: {quality}, 比例: {}, 去水印: {no_watermark}, 单张超时: {}ms",
        total,
        ratio.as_deref().unwrap_or("默认"),
        timeout_ms
    );

    // 2. 初始化浏览器（无头失败时降级到 UI 模式，复用单图的降级逻辑）
    let mut client = DoubaoClient::new()?;
    match client.init(headless).await {
        Ok(()) => {}
        Err(e) if headless => {
            println!("\n⚠️ 无头模式初始化失败: {e}");
            client.close().await;
            println!("=============================================");
            println!("🔄 正在自动以 UI 模式重启...");
            println!("💡 如果出现验证码或登录页，请在浏览器中手动完成。");
            println!("=============================================\n");
            client = DoubaoClient::new()?;
            if let Err(e) = client.init(false).await {
                // init 失败也要关闭已启动的浏览器进程，避免残留
                client.close().await;
                return Err(e);
            }
        }
        Err(e) => return Err(e),
    }

    // 3. 可选的上下文消息：纯文字，不等图片，等 AI 回复停止或 30s 超时后继续
    if let Some(ctx) = plan
        .context
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        println!("\n===== 发送上下文消息（{} 字） =====", ctx.chars().count());
        if let Err(e) = client.send_context_message(ctx, 30_000).await {
            eprintln!("⚠️ 上下文消息发送/等待失败（继续批量生图）: {e}");
        }
    }

    // 4. 逐条发送生图 prompt（同一对话），失败记录并继续
    let mut results: Vec<BatchResultItem> = Vec::with_capacity(total);
    for (i, item) in plan.items.iter().enumerate() {
        println!("\n===== 批量生图 [{}/{}] =====", i + 1, total);
        println!("Prompt: {}", item.prompt);
        println!("Output: {}", item.output.display());

        let result =
            batch_generate_one(&mut client, item, &quality, ratio.as_deref(), timeout_ms, no_watermark)
                .await;
        if result.ok {
            println!("✅ [{}/{}] 成功: {}", i + 1, total, item.output.display());
        } else {
            eprintln!(
                "❌ [{}/{}] 失败: {} — {}",
                i + 1,
                total,
                item.output.display(),
                result.error.as_deref().unwrap_or("未知错误")
            );
        }
        results.push(result);
    }

    client.close().await;

    // 5. stdout 输出一行 JSON 摘要（供下游解析）；全部失败才非零退出
    let ok_count = results.iter().filter(|r| r.ok).count();
    println!("\n===== 批量生图完成: {ok_count}/{total} 成功 =====");
    let summary = serde_json::json!({ "results": results });
    println!("{}", serde_json::to_string(&summary)?);

    if ok_count == 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// 执行单个批量 item：发送 prompt → 等图 → 下载 → 按需去水印。
async fn batch_generate_one(
    client: &mut DoubaoClient,
    item: &BatchItem,
    quality: &str,
    ratio: Option<&str>,
    timeout_ms: u64,
    no_watermark: bool,
) -> BatchResultItem {
    let output_str = item.output.to_string_lossy().to_string();
    let fail = |msg: String| BatchResultItem {
        output: output_str.clone(),
        ok: false,
        error: Some(msg),
    };

    let image_info = match client
        .generate_image(&item.prompt, quality, ratio, timeout_ms, &[])
        .await
    {
        Ok(Some(info)) => info,
        // 首轮取图失败（超时/取 URL 失败）：在同一对话里补发一次「重新生成」再取
        Ok(None) => {
            println!("⚠️ 首轮未等到图片，在同一对话补发一次重新生成...");
            match client.regenerate_and_wait(quality, timeout_ms).await {
                Ok(Some(info)) => info,
                Ok(None) => {
                    return fail(format!("等待图片超时（{timeout_ms}ms，含一次补发重试）"));
                }
                Err(e) => return fail(format!("补发重试失败: {e}")),
            }
        }
        Err(e) => {
            println!("⚠️ 首轮取图出错: {e}，在同一对话补发一次重新生成...");
            match client.regenerate_and_wait(quality, timeout_ms).await {
                Ok(Some(info)) => info,
                Ok(None) => return fail(format!("{e}；补发后仍未等到图片")),
                Err(e2) => return fail(format!("{e}；补发重试失败: {e2}")),
            }
        }
    };
    println!(
        "图片链接: {} {}",
        image_info.url,
        if image_info.is_watermark_free {
            "(无水印原图)"
        } else {
            ""
        }
    );

    let saved = match client.download_with_page(&image_info.url, &item.output).await {
        Ok(p) => p,
        Err(e) => return fail(format!("下载失败: {e}")),
    };

    // 与单图模式一致：仅当 URL 不是 SSE 拦截到的无水印原图时才做本地裁切去水印
    if no_watermark && !image_info.is_watermark_free {
        match remove_watermark(&saved) {
            Ok(()) => println!("🧹 水印已去除"),
            Err(e) => eprintln!("⚠️ 水印去除失败: {e}"),
        }
    } else if no_watermark && image_info.is_watermark_free {
        println!("🧹 已直接下载无水印原图，无需额外处理");
    }

    BatchResultItem {
        output: output_str,
        ok: true,
        error: None,
    }
}
