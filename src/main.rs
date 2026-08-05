use anyhow::Result;
use clap::Parser;
use doubao_web_image::client::DoubaoClient;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "doubao-web-image")]
#[command(about = "豆包 Web 端自动化生图工具 (Rust + chromiumoxide)")]
#[command(version = "1.3.0")]
struct Args {
    /// 生图提示词
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

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

选项:
    --ui                    显示浏览器窗口（首次登录必须带此参数）
    --quality=<QUALITY>     图片质量: preview 或 original (默认: original)
    --ratio=<RATIO>         图片比例 (如: 16:9, 1:1, 9:16, 2:3, 3:4, 4:3)
    --output=<PATH>         输出文件路径 (默认: generated.png)
    --image=<PATH>          --output 的别名
    --reference=<PATH>      参考图路径（可重复，最多 4 张；支持逗号分隔）
    --no-watermark          去除左上角水印（AI 生成标签）
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

    let image_info = client
        .generate_image(prompt, quality, ratio, 120_000, references)
        .await?
        .ok_or_else(|| anyhow::anyhow!("未能获取图片 URL"))?;

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
