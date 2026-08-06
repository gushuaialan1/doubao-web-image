use anyhow::{Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chromiumoxide::Page;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::dom::SetFileInputFilesParams;
use chromiumoxide::cdp::browser_protocol::network::{EventResponseReceived, GetResponseBodyParams};
use chromiumoxide::cdp::browser_protocol::page::{
    AddScriptToEvaluateOnNewDocumentParams, NavigateParams,
};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::time::sleep;

use crate::stealth;

/// 注入到页面的 EventStream 拦截脚本。
///
/// 借鉴 doubao-nomark 的浏览器插件思路：重写 `window.fetch`（并兜底 `XMLHttpRequest`），
/// 截获 `/chat/completion` 的 SSE 流并解析其中的 `image_ori_raw` 字段，
/// 从而直接拿到豆包生成的无水印原图链接。
const STREAM_INTERCEPTOR_SCRIPT: &str = r#"
(() => {
    if (window.__doubaoInterceptorInstalled) return;
    window.__doubaoInterceptorInstalled = true;
    window.__doubaoImageOriRaws = window.__doubaoImageOriRaws || [];
    window.__doubaoInterceptorDebug = {
        fetchSeen: 0,
        xhrSeen: 0,
        matchedFetch: 0,
        matchedXhr: 0,
        parseErrors: 0,
        lastError: null,
    };

    function getUrlString(input) {
        if (typeof input === 'string') return input;
        if (input && typeof input.url === 'string') return input.url;
        if (input && typeof input.toString === 'function') return input.toString();
        return '';
    }

    function tryParseImageOriRaw(data) {
        try {
            let creations = [];
            if (Array.isArray(data.patch_op)) {
                for (const op of data.patch_op) {
                    if (op.patch_value && Array.isArray(op.patch_value.content_block)) {
                        for (const block of op.patch_value.content_block) {
                            const cb = block?.content?.creation_block;
                            if (cb && Array.isArray(cb.creations)) {
                                creations = cb.creations;
                                break;
                            }
                        }
                    }
                    if (creations.length) break;
                }
                if (!creations.length) {
                    const extPatch = data.patch_op.find(op =>
                        op.patch_value && typeof op.patch_value === 'object' && op.patch_value.ext?.creation_full_content
                    );
                    if (extPatch) {
                        try {
                            const full = JSON.parse(extPatch.patch_value.ext.creation_full_content);
                            for (const item of full) {
                                const content = item?.BlockInfo?.BlockContent?.content;
                                if (content && content.creation_block && Array.isArray(content.creation_block.creations)) {
                                    creations = content.creation_block.creations;
                                    break;
                                }
                            }
                        } catch (e) {}
                    }
                }
            } else if (data.event_data) {
                try {
                    const eventData = JSON.parse(data.event_data);
                    if (eventData.message?.content) {
                        const messageContent = JSON.parse(eventData.message.content);
                        if (messageContent.creations && Array.isArray(messageContent.creations)) {
                            creations = messageContent.creations;
                        }
                    }
                } catch (e) {}
            }

            for (const creation of creations) {
                const imageData = creation?.image?.image_ori_raw;
                if (!imageData) continue;
                let imageUrl = '';
                let width = 0;
                let height = 0;
                if (typeof imageData === 'string') {
                    imageUrl = imageData;
                } else if (typeof imageData === 'object' && imageData.url) {
                    imageUrl = imageData.url;
                    width = imageData.width || 0;
                    height = imageData.height || 0;
                }
                if (imageUrl && !window.__doubaoImageOriRaws.find(img => img.url === imageUrl)) {
                    window.__doubaoImageOriRaws.push({
                        url: imageUrl.replace(/&amp;/g, '&'),
                        width: width,
                        height: height
                    });
                }
            }
        } catch (e) {
            window.__doubaoInterceptorDebug.parseErrors += 1;
            window.__doubaoInterceptorDebug.lastError = String(e);
        }
    }

    // Buffer for SSE lines that span multiple fetch chunks.
    let sseBuffer = '';
    function feedSse(text) {
        sseBuffer += text;
        const lines = sseBuffer.split('\n');
        // Keep the last (possibly incomplete) line in the buffer.
        sseBuffer = lines.pop() || '';
        for (const line of lines) {
            const trimmed = line.trim();
            if (!trimmed || trimmed === '[DONE]') continue;
            // Match both "data: {...}" and "data:{...}"
            let jsonStr = '';
            if (trimmed.startsWith('data:')) {
                jsonStr = trimmed.substring(5).trim();
            }
            if (!jsonStr || !jsonStr.includes('image_ori')) continue;
            try {
                const data = JSON.parse(jsonStr);
                tryParseImageOriRaw(data);
            } catch (e) {
                window.__doubaoInterceptorDebug.parseErrors += 1;
                window.__doubaoInterceptorDebug.lastError = String(e);
            }
        }
    }

    // ===== Intercept fetch =====
    const originalFetch = window.fetch;
    window.fetch = async function(...args) {
        const url = getUrlString(args[0]);
        window.__doubaoInterceptorDebug.fetchSeen += 1;
        if (url && url.includes('/chat/completion')) {
            window.__doubaoInterceptorDebug.matchedFetch += 1;
            try {
                const response = await originalFetch.apply(this, args);
                if (!response.body) return response;
                const reader = response.body.getReader();
                const decoder = new TextDecoder();
                const stream = new ReadableStream({
                    async start(controller) {
                        while (true) {
                            const { done, value } = await reader.read();
                            if (done) break;
                            const chunk = decoder.decode(value, { stream: true });
                            feedSse(chunk);
                            controller.enqueue(value);
                        }
                        controller.close();
                    }
                });
                return new Response(stream, {
                    headers: response.headers,
                    status: response.status,
                    statusText: response.statusText
                });
            } catch (e) {
                window.__doubaoInterceptorDebug.lastError = String(e);
                return originalFetch.apply(this, args);
            }
        }
        return originalFetch.apply(this, args);
    };

    // ===== Intercept XMLHttpRequest as fallback =====
    const originalXHROpen = XMLHttpRequest.prototype.open;
    const originalXHRSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function(method, url, ...rest) {
        this._url = getUrlString(url);
        return originalXHROpen.apply(this, [method, url, ...rest]);
    };
    XMLHttpRequest.prototype.send = function(...args) {
        const url = this._url || '';
        window.__doubaoInterceptorDebug.xhrSeen += 1;
        if (url && url.includes('/chat/completion')) {
            window.__doubaoInterceptorDebug.matchedXhr += 1;
            this.addEventListener('load', function() {
                try {
                    feedSse(this.responseText);
                } catch (e) {
                    window.__doubaoInterceptorDebug.lastError = String(e);
                }
            });
        }
        return originalXHRSend.apply(this, args);
    };
})();
"#;

#[derive(Debug, serde::Deserialize, Clone)]
struct ImageOriRawItem {
    url: String,
    width: u32,
    height: u32,
}

#[derive(Debug, serde::Deserialize, Default)]
#[allow(dead_code)]
struct InterceptorDebug {
    #[serde(default)]
    fetch_seen: u32,
    #[serde(default)]
    xhr_seen: u32,
    #[serde(default)]
    matched_fetch: u32,
    #[serde(default)]
    matched_xhr: u32,
    #[serde(default)]
    parse_errors: u32,
    #[serde(default)]
    last_error: Option<String>,
}

/// 生成结果信息。
#[derive(Debug, Clone)]
pub struct GeneratedImageInfo {
    /// 图片下载地址。
    pub url: String,
    /// 是否为通过 SSE 拦截到的无水印原图（`image_ori_raw`）。
    pub is_watermark_free: bool,
}

pub struct DoubaoClient {
    browser: Option<Browser>,
    page: Option<Arc<Page>>,
    user_data_dir: PathBuf,
    intercepted_buffers: HashMap<String, Vec<u8>>,
}

impl DoubaoClient {
    pub fn new() -> Result<Self> {
        let home = directories::BaseDirs::new()
            .ok_or_else(|| anyhow!("Cannot determine home directory"))?
            .home_dir()
            .to_path_buf();
        let user_data_dir = home.join(".doubao-web-session");
        std::fs::create_dir_all(&user_data_dir)?;

        Ok(Self {
            browser: None,
            page: None,
            user_data_dir,
            intercepted_buffers: HashMap::new(),
        })
    }

    pub async fn init(&mut self, headless: bool) -> Result<()> {
        println!("[DoubaoClient] Initializing browser (headless: {headless})...");
        println!(
            "[DoubaoClient] User data directory: {}",
            self.user_data_dir.display()
        );

        let viewport = stealth::random_viewport();

        let mut config_builder = BrowserConfig::builder()
            .viewport(viewport)
            .user_data_dir(self.user_data_dir.clone())
            .args(stealth::build_stealth_args());
        if !headless {
            config_builder = config_builder.with_head();
        }
        let config = config_builder.build().map_err(|e| anyhow!("{e}"))?;

        let (browser, mut handler) = Browser::launch(config).await?;

        // Spawn browser event handler
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        // Create blank page first, apply stealth, then navigate to target
        let page = Arc::new(browser.new_page("about:blank").await?);

        // Enable chromiumoxide built-in stealth + custom UA
        page.enable_stealth_mode_with_agent(stealth::USER_AGENT)
            .await?;

        // Register supplemental stealth script for all future documents/iframes
        page.execute(AddScriptToEvaluateOnNewDocumentParams::new(
            stealth::STEALTH_SCRIPT,
        ))
        .await?;

        // Also inject into current blank page immediately
        let _ = page.evaluate(stealth::STEALTH_SCRIPT).await?;

        // Inject EventStream interceptor to capture watermark-free image_ori_raw URLs
        page.execute(AddScriptToEvaluateOnNewDocumentParams::new(
            STREAM_INTERCEPTOR_SCRIPT,
        ))
        .await?;
        let _ = page.evaluate(STREAM_INTERCEPTOR_SCRIPT).await?;

        // Navigate to Doubao
        page.goto(NavigateParams::new("https://www.doubao.com/chat/"))
            .await?;
        page.wait_for_navigation().await?;
        sleep(Duration::from_millis(3000)).await;

        let url: String = page.evaluate("window.location.href").await?.into_value()?;
        let title: String = page.evaluate("document.title").await?.into_value()?;
        println!("[DoubaoClient-Debug] URL: {url}");
        println!("[DoubaoClient-Debug] Title: {title}");

        self.browser = Some(browser);
        self.page = Some(Arc::clone(&page));

        // Check login state
        let has_login_modal = url.contains("login");
        let login_text_visible: bool = page
            .evaluate(
                r#"
                Array.from(document.querySelectorAll('button, a, div, span'))
                    .some(el => el.textContent.includes('登录') && el.offsetParent !== null)
            "#,
            )
            .await?
            .into_value()?;

        if has_login_modal || login_text_visible {
            println!("\n=============================================");
            println!("需要登录豆包");

            if headless {
                println!("当前处于无头模式，无法进行手动登录。");
                println!("请运行带 --ui 参数的命令进行首次登录");
                println!("=============================================\n");
                return Err(anyhow!("Login required but running in headless mode"));
            }

            println!("请在打开的浏览器窗口中完成登录。");
            println!("=============================================\n");

            // Wait for textarea to appear (login successful)
            println!("[DoubaoClient] 等待用户登录...");
            self.wait_for_element("textarea", 0).await?;
            println!("[DoubaoClient] 检测到输入框，登录成功！继续执行。");
        } else {
            println!("[DoubaoClient] 已检测到登录状态。");
        }
        Ok(())
    }

    /// 单图模式入口：发送生图 prompt 并等待新图，返回最优图片 URL。
    ///
    /// 内部拆为「发送前快照 → send_message → wait_for_new_image」三步，
    /// 批量模式（--batch）复用同一套逻辑在同一对话中逐条发送。
    pub async fn generate_image(
        &mut self,
        prompt: &str,
        quality: &str,
        ratio: Option<&str>,
        timeout_ms: u64,
        references: &[PathBuf],
    ) -> Result<Option<GeneratedImageInfo>> {
        let final_prompt = match ratio {
            Some(r) => format!("{prompt}，图片比例 {r}"),
            None => prompt.to_string(),
        };

        println!("[DoubaoClient] 发送生图请求: {final_prompt} (质量: {quality})");

        // Clear previous intercepts
        self.intercepted_buffers.clear();

        // Start network interception (captures original image responses for this turn)
        let mut intercept_task = self.spawn_response_interceptor()?;

        // Collect existing image URLs / SSE cache count BEFORE sending so the wait
        // logic can diff against what appears afterwards. Works in a shared
        // conversation that already contains older images (batch mode).
        let before_urls = self.current_image_urls().await?;
        println!(
            "[DoubaoClient-Debug] 发送指令前，已有图片数量: {}",
            before_urls.len()
        );
        let before_ori_count = self.ori_raw_count().await.unwrap_or(0);
        println!(
            "[DoubaoClient-Debug] 发送指令前，无水印原图缓存: {}",
            before_ori_count
        );

        // Upload references (if any), fill textarea and press Enter
        self.send_message(&format!("帮我生成图片：{final_prompt}"), references)
            .await?;
        println!("[DoubaoClient] 已发送指令，等待图片生成...");

        self.wait_for_new_image(
            &mut intercept_task,
            &before_urls,
            before_ori_count,
            quality,
            timeout_ms,
        )
        .await
    }

    /// 在当前对话中发送一条消息（可选先上传参考图），立即返回，不等待任何回复。
    ///
    /// 单图模式与批量模式（context 消息、逐条生图 prompt）都通过它发送，
    /// 因此批量模式下所有消息都落在同一个对话里。
    pub async fn send_message(&self, text: &str, references: &[PathBuf]) -> Result<()> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;

        // Upload reference images before typing the prompt
        if !references.is_empty() {
            self.upload_references(references).await?;
        }

        // Find and fill textarea (acquire after upload: React may re-render the input area)
        let textarea = self.wait_for_element("textarea", 10000).await?;
        textarea.click().await?;
        sleep(Duration::from_millis(200)).await;

        // Insert text via CDP Input.insertText (triggers React onChange, send button appears)
        page.execute(
            chromiumoxide::cdp::browser_protocol::input::InsertTextParams::new(text),
        )
        .await?;
        sleep(Duration::from_millis(500)).await;

        // Press Enter to send via CDP Input.dispatchKeyEvent (more reliable than element.press_key)
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType,
        };
        page.execute(
            DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyDown)
                .key("Enter")
                .code("Enter")
                .windows_virtual_key_code(13)
                .native_virtual_key_code(13)
                .build()
                .unwrap(),
        )
        .await?;
        page.execute(
            DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key("Enter")
                .code("Enter")
                .windows_virtual_key_code(13)
                .native_virtual_key_code(13)
                .build()
                .unwrap(),
        )
        .await?;
        Ok(())
    }

    /// 发送纯文本上下文消息（不期待图片产出），给豆包建立全文上下文。
    ///
    /// 发送后等待 AI 回复「开始并停止」：先等页面文本长度增长（回复开始，最多 10s），
    /// 再等文本长度连续 3s 不再变化（回复停止）。无论回复是否完整，最多等待
    /// timeout_ms 后返回；全程不触发任何等图逻辑。
    pub async fn send_context_message(&mut self, text: &str, timeout_ms: u64) -> Result<()> {
        let page = Arc::clone(
            self.page
                .as_ref()
                .ok_or_else(|| anyhow!("Not initialized"))?,
        );

        self.send_message(text, &[]).await?;
        println!(
            "[DoubaoClient] 上下文消息已发送，等待 AI 回复停止（最长 {}s）...",
            timeout_ms / 1000
        );

        // 等本地回显渲染完再取基线，避免把自己消息的渲染误判成 AI 回复开始
        sleep(Duration::from_millis(1500)).await;
        let baseline: usize = page
            .evaluate("document.body.innerText.length")
            .await?
            .into_value()?;

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        // Phase 1: 等 AI 回复开始（最多 10s，且不超过总 deadline）
        let start_deadline = (Instant::now() + Duration::from_secs(10)).min(deadline);
        let mut reply_started = false;
        while Instant::now() < start_deadline {
            sleep(Duration::from_millis(1000)).await;
            let len: usize = page
                .evaluate("document.body.innerText.length")
                .await?
                .into_value()?;
            if len > baseline {
                reply_started = true;
                break;
            }
        }
        if !reply_started {
            println!("[DoubaoClient] 未检测到文字回复，直接继续后续流程");
            return Ok(());
        }
        println!("[DoubaoClient] 检测到 AI 开始回复，等待回复停止...");

        // Phase 2: 等回复停止（连续 3 轮页面文本长度不变视为停止）
        let mut last_len = 0usize;
        let mut stable_rounds = 0u32;
        while Instant::now() < deadline {
            sleep(Duration::from_millis(1000)).await;
            let len: usize = page
                .evaluate("document.body.innerText.length")
                .await?
                .into_value()?;
            if len == last_len {
                stable_rounds += 1;
                if stable_rounds >= 3 {
                    println!("[DoubaoClient] AI 回复已停止，继续后续流程");
                    return Ok(());
                }
            } else {
                stable_rounds = 0;
                last_len = len;
            }
        }

        println!(
            "[DoubaoClient] 等待回复超时（{}s），继续后续流程",
            timeout_ms / 1000
        );
        Ok(())
    }

    /// 当前对话 DOM 中所有已渲染生图缩略图的 URL 列表。
    async fn current_image_urls(&self) -> Result<Vec<String>> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        let urls: Vec<String> = page
            .evaluate(
                r#"
                Array.from(document.querySelectorAll('img[src*="flow-imagex-sign"]'))
                    .map(img => img.getAttribute('src'))
            "#,
            )
            .await?
            .into_value()?;
        Ok(urls)
    }

    /// 启动网络响应拦截任务：捕获本轮回合中 flow-imagex-sign / image_pre_watermark
    /// 的响应体，供 download_with_page 直接使用内存数据保存原图。
    ///
    /// 返回的 JoinHandle 由调用方在等待结束后收集并 abort，避免批量模式下任务堆积。
    fn spawn_response_interceptor(
        &self,
    ) -> Result<tokio::task::JoinHandle<HashMap<String, Vec<u8>>>> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        let page_arc = Arc::clone(page);
        Ok(tokio::spawn(async move {
            let mut events = match page_arc.event_listener::<EventResponseReceived>().await {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[DoubaoClient] Failed to attach event listener: {e}");
                    return HashMap::<String, Vec<u8>>::new();
                }
            };

            let mut buffers = HashMap::new();
            while let Some(event) = events.next().await {
                let url = &event.response.url;
                if url.contains("flow-imagex-sign") || url.contains("image_pre_watermark") {
                    match page_arc
                        .execute(GetResponseBodyParams::new(event.request_id.clone()))
                        .await
                    {
                        Ok(body) => {
                            let data = if body.base64_encoded {
                                match STANDARD.decode(&body.body) {
                                    Ok(d) => d,
                                    Err(_) => body.body.as_bytes().to_vec(),
                                }
                            } else {
                                body.body.as_bytes().to_vec()
                            };
                            println!(
                                "[DoubaoClient] 网络拦截: 捕获原图响应 ({} bytes)",
                                data.len()
                            );
                            buffers.insert(url.clone(), data);
                        }
                        Err(e) => {
                            eprintln!("[DoubaoClient] Failed to get response body: {e}");
                        }
                    }
                }
            }
            buffers
        }))
    }

    /// 等待当前对话中出现「发送前快照之后新增」的图片，并按 quality 提取最优 URL。
    ///
    /// before_urls / before_ori_count 由调用方在发送消息前快照，同一对话里已有的
    /// 旧图不会被误判为新图（批量模式的关键）。如果豆包一次回复多张候选图，
    /// 沿用既有策略取最新（最后）一张。
    async fn wait_for_new_image(
        &mut self,
        intercept_task: &mut tokio::task::JoinHandle<HashMap<String, Vec<u8>>>,
        before_urls: &[String],
        before_ori_count: usize,
        quality: &str,
        timeout_ms: u64,
    ) -> Result<Option<GeneratedImageInfo>> {
        let page = Arc::clone(
            self.page
                .as_ref()
                .ok_or_else(|| anyhow!("Not initialized"))?,
        );

        // Enforce the overall timeout across both SSE interception and DOM polling.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);

        // Try to get watermark-free original URL from intercepted EventStream first.
        // This mirrors the approach used by doubao-nomark: parse /chat/completion SSE
        // and read creation.image.image_ori_raw.
        if quality != "preview" {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_millis() as u64;
            let ori_timeout = remaining.saturating_sub(20_000).max(30_000);
            if let Some(item) = self.poll_new_ori_raw(before_ori_count, ori_timeout).await? {
                println!("[DoubaoClient] 已获取无水印原图 URL，跳过模态框提取");
                intercept_task.abort();
                return Ok(Some(GeneratedImageInfo {
                    url: item.url,
                    is_watermark_free: true,
                }));
            }
            println!("[DoubaoClient] SSE 未拦截到无水印原图，回退到 DOM 提取");
        }

        // Poll for NEW images (by URL diff, not just count)
        let mut target_url: Option<String> = None;
        let mut poll_count = 0;

        while Instant::now() < deadline {
            sleep(Duration::from_millis(2000)).await;
            poll_count += 1;

            let current_urls = self.current_image_urls().await?;
            println!(
                "[DoubaoClient-Debug] 第 {poll_count} 次轮询, 当前图片数量: {}",
                current_urls.len()
            );

            // Find URLs that appeared after we sent the message
            let new_urls: Vec<String> = current_urls
                .iter()
                .filter(|url| !before_urls.contains(url))
                .cloned()
                .collect();

            if !new_urls.is_empty() {
                // Take the last new URL (most likely the newest generated image)
                let newest = new_urls.last().unwrap().clone();
                sleep(Duration::from_millis(3000)).await;
                target_url = Some(newest);
                println!(
                    "[DoubaoClient] 检测到新图片生成 (新增 {} 张)",
                    new_urls.len()
                );
                break;
            }
        }

        let target_url = match target_url {
            Some(u) => u,
            None => {
                println!("[DoubaoClient] 等待图片超时");
                intercept_task.abort();
                return Ok(None);
            }
        };

        // If preview only, return immediately
        if quality == "preview" {
            intercept_task.abort();
            return Ok(Some(GeneratedImageInfo {
                url: target_url,
                is_watermark_free: false,
            }));
        }

        // === Get original image ===
        println!("[DoubaoClient] 正在尝试获取原始大图...");

        // 1. Click thumbnail to open modal
        let click_script = format!(
            r#"
            (function() {{
                const imgs = document.querySelectorAll('img[src*="flow-imagex-sign"]');
                for (let i = imgs.length - 1; i >= 0; i--) {{
                    if (imgs[i].getAttribute('src').includes('{}')) {{
                        imgs[i].click();
                        return true;
                    }}
                }}
                return false;
            }})()
            "#,
            &target_url[..target_url.len().min(30)]
        );
        page.evaluate(click_script.as_str()).await?;
        sleep(Duration::from_millis(3000)).await;
        println!("[DoubaoClient] 已打开大图模态框");

        // 2. Click save button
        let clicked = self.click_save_button().await?;
        if clicked {
            println!("[DoubaoClient] 已点击保存按钮");
        } else {
            println!("[DoubaoClient] 未找到保存按钮，直接提取 URL");
        }

        sleep(Duration::from_millis(2000)).await;

        // 3. Extract original URL from DOM
        let best_url: Option<String> = page
            .evaluate(r#"
                (function() {
                    const imgs = document.querySelectorAll('img[src*="flow-imagex-sign"]');
                    for (const img of imgs) {
                        const src = img.getAttribute('src');
                        if (src && src.includes('image_pre_watermark')) {
                            return src;
                        }
                    }
                    for (const img of imgs) {
                        const src = img.getAttribute('src');
                        if (src && !src.includes('downsize') && !src.includes('web-operation') && !src.includes('avatar')) {
                            return src;
                        }
                    }
                    return null;
                })()
            "#)
            .await?
            .into_value()?;

        // 4. Collect intercepted buffers, then stop the interceptor for this turn
        sleep(Duration::from_millis(1000)).await;
        let intercepted =
            match tokio::time::timeout(Duration::from_secs(2), &mut *intercept_task).await {
                Ok(Ok(bufs)) => bufs,
                _ => HashMap::new(),
            };
        intercept_task.abort();
        self.intercepted_buffers = intercepted;

        if !self.intercepted_buffers.is_empty() {
            let (first_url, first_buf) = self.intercepted_buffers.iter().next().unwrap();
            let _ = first_url;
            println!(
                "[DoubaoClient] 网络拦截捕获到原图 ({} bytes)",
                first_buf.len()
            );
        }

        // 5. Close modal
        page.evaluate(
            r#"
            document.dispatchEvent(new KeyboardEvent('keydown', {
                key: 'Escape', code: 'Escape', keyCode: 27, bubbles: true
            }));
            true
        "#,
        )
        .await?;
        sleep(Duration::from_millis(500)).await;

        if let Some(url) = best_url {
            println!(
                "[DoubaoClient] 最终原图 URL: {}...",
                &url[..url.len().min(80)]
            );
            return Ok(Some(GeneratedImageInfo {
                url,
                is_watermark_free: false,
            }));
        }

        println!("[DoubaoClient] 未能获取原图，回退到缩略图");
        Ok(Some(GeneratedImageInfo {
            url: target_url,
            is_watermark_free: false,
        }))
    }

    /// 上传参考图到聊天附件区。
    ///
    /// 豆包前端是 React SPA，文件输入框隐藏、且只在点击输入区「+」按钮后才挂载到 DOM，
    /// 无法通过点击触发文件选择对话框（headless 下无法处理），因此流程为：
    /// 1. 真实点击「+」按钮（CDP 鼠标事件，Radix 菜单依赖 pointer 事件）；
    /// 2. 等待 `<input type="file">` 出现，用 CDP `DOM.setFileInputFiles` 注入文件路径；
    /// 3. 等待缩略图（blob:）出现且上传进度（semi-progress-circle / loading-overlay）消失。
    async fn upload_references(&self, paths: &[PathBuf]) -> Result<()> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        println!("[DoubaoClient] 正在上传 {} 张参考图...", paths.len());

        // 确保页面已就绪
        self.wait_for_element("textarea", 10000).await?;

        // file input 未挂载时，先点击「+」按钮触发挂载
        if page.find_element("input[type=\"file\"]").await.is_err() {
            let tag_plus = r#"
(function() {
    const ta = document.querySelector('textarea');
    if (!ta) return false;
    let container = ta;
    for (let i = 0; i < 7 && container.parentElement; i++) container = container.parentElement;
    for (const btn of container.querySelectorAll('button')) {
        const path = btn.querySelector('svg path');
        if (path && (path.getAttribute('d') || '').startsWith('M12.0005 2.25')) {
            btn.id = '__doubao_plus_btn';
            return true;
        }
    }
    return false;
})()
"#;
            let tagged: bool = page.evaluate(tag_plus).await?.into_value()?;
            if tagged {
                let plus = page.find_element("#__doubao_plus_btn").await?;
                plus.click().await?;
                println!("[DoubaoClient] 已点击「+」按钮");
            } else {
                println!("[DoubaoClient] 未定位到「+」按钮，尝试直接等待文件输入框");
            }
        }

        // 等待文件输入框出现
        let input = self
            .wait_for_element("input[type=\"file\"]", 10000)
            .await
            .map_err(|_| anyhow!("未找到文件上传输入框（豆包页面结构可能已变更）"))?;

        // 通过 CDP 注入文件路径（input multiple=true，一次注入全部）
        let files: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let mut params = SetFileInputFilesParams::new(files);
        params.node_id = Some(input.node_id);
        page.execute(params).await?;
        println!("[DoubaoClient] 已注入参考图文件，等待上传完成...");

        // 等待缩略图出现且上传进度消失（连续 2 轮稳定视为完成）
        let expected = paths.len();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut stable_rounds = 0u32;
        loop {
            if Instant::now() > deadline {
                return Err(anyhow!("参考图上传超时（60s）"));
            }
            sleep(Duration::from_millis(1000)).await;
            let status: serde_json::Value = page
                .evaluate(
                    r#"
(function() {
    const ta = document.querySelector('textarea');
    if (!ta) return {thumbs: 0, uploading: false};
    let container = ta;
    for (let i = 0; i < 9 && container.parentElement; i++) container = container.parentElement;
    const thumbs = container.querySelectorAll('img[src^="blob:"]').length;
    const uploading = container.querySelectorAll(
        '.semi-progress-circle, [class*="loading-overlay"], [class*="progress-text"]'
    ).length > 0;
    return {thumbs, uploading};
})()
"#,
                )
                .await?
                .into_value()?;
            let thumbs = status["thumbs"].as_u64().unwrap_or(0) as usize;
            let uploading = status["uploading"].as_bool().unwrap_or(false);
            println!(
                "[DoubaoClient-Debug] 参考图上传状态: 缩略图 {thumbs}/{expected}, 上传中: {uploading}"
            );
            if thumbs >= expected && !uploading {
                stable_rounds += 1;
                if stable_rounds >= 2 {
                    println!("[DoubaoClient] 参考图上传完成");
                    return Ok(());
                }
            } else {
                stable_rounds = 0;
            }
        }
    }

    async fn click_save_button(&self) -> Result<bool> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;

        // Strategy A: find button by text content
        for text in ["保存", "下载", "Save", "Download"] {
            let script = format!(
                r#"
                (function() {{
                    const buttons = document.querySelectorAll('button, [role="button"]');
                    for (const btn of buttons) {{
                        if (btn.textContent.trim() === '{}' && btn.offsetParent !== null) {{
                            btn.click();
                            return true;
                        }}
                    }}
                    return false;
                }})()
                "#,
                text
            );
            let found: bool = page.evaluate(script.as_str()).await?.into_value()?;
            if found {
                return Ok(true);
            }
        }

        // Strategy B: find by aria-label
        for label in ["保存", "下载", "save", "download"] {
            let script = format!(
                r#"
                (function() {{
                    const btn = document.querySelector('[aria-label="{}" i]');
                    if (btn && btn.offsetParent !== null) {{
                        btn.click();
                        return true;
                    }}
                    return false;
                }})()
                "#,
                label
            );
            let found: bool = page.evaluate(script.as_str()).await?.into_value()?;
            if found {
                return Ok(true);
            }
        }

        // Strategy C: find SVG download icon by path
        let found: bool = page
            .evaluate(r#"
                (function() {
                    const svgs = document.querySelectorAll('svg');
                    for (const svg of svgs) {
                        const html = svg.outerHTML;
                        if (html.includes('M19.207 12.707') || html.includes('M2 19C2') || html.includes('download') || html.includes('M4 16v')) {
                            let el = svg.parentElement;
                            while (el && el.tagName !== 'BUTTON' && el.getAttribute('role') !== 'button') {
                                el = el.parentElement;
                                if (el && el.tagName === 'DIV' && window.getComputedStyle(el).cursor === 'pointer') {
                                    break;
                                }
                            }
                            if (el) {
                                el.click();
                                return true;
                            }
                        }
                    }
                    return false;
                })()
            "#)
            .await?
            .into_value()?;

        Ok(found)
    }

    pub async fn download_with_page(&self, url: &str, dest: &PathBuf) -> Result<PathBuf> {
        if let Some(data) = self.intercepted_buffers.get(url) {
            println!("[DoubaoClient] 使用浏览器拦截的原图数据保存...");
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(dest, data).await?;
            println!(
                "[DoubaoClient] 图片已保存至: {} ({} bytes)",
                dest.display(),
                data.len()
            );
            return Ok(dest.clone());
        }

        Self::download_image(url, dest).await
    }

    pub async fn download_image(url: &str, dest: &PathBuf) -> Result<PathBuf> {
        println!("[DoubaoClient] 正在下载图片至: {}", dest.display());
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }

        let client = reqwest::Client::new();
        let resp = client
            .get(url)
            .header("Referer", "https://www.doubao.com/")
            .header("User-Agent", stealth::USER_AGENT)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Download failed: HTTP {}", resp.status()));
        }

        let data = resp.bytes().await?;
        fs::write(dest, &data).await?;
        println!(
            "[DoubaoClient] 图片已保存至: {} ({} bytes)",
            dest.display(),
            data.len()
        );
        Ok(dest.clone())
    }

    pub async fn close(&mut self) {
        if let Some(mut browser) = self.browser.take() {
            let _ = browser.close().await;
            println!("[DoubaoClient] 浏览器已关闭。");
        }
    }

    /// 当前已缓存的无水印原图数量。
    async fn ori_raw_count(&self) -> Result<usize> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        let count: usize = page
            .evaluate("window.__doubaoImageOriRaws ? window.__doubaoImageOriRaws.length : 0")
            .await?
            .into_value()?;
        Ok(count)
    }

    /// 等待并返回新增的 `image_ori_raw` 条目（水印-free 原图）。
    async fn poll_new_ori_raw(
        &self,
        before_count: usize,
        timeout_ms: u64,
    ) -> Result<Option<ImageOriRawItem>> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut poll_count = 0;

        while Instant::now() < deadline {
            sleep(Duration::from_millis(1500)).await;
            poll_count += 1;

            let items: Vec<ImageOriRawItem> = page
                .evaluate("window.__doubaoImageOriRaws || []")
                .await?
                .into_value()?;

            if items.len() > before_count {
                if let Some(item) = items.last() {
                    println!(
                        "[DoubaoClient] SSE 拦截到无水印原图 ({}x{}): {}...",
                        item.width,
                        item.height,
                        &item.url[..item.url.len().min(60)]
                    );
                    return Ok(Some(item.clone()));
                }
            }

            if poll_count % 4 == 0 {
                let debug: InterceptorDebug = page
                    .evaluate("window.__doubaoInterceptorDebug || {}")
                    .await?
                    .into_value()
                    .unwrap_or_default();
                println!(
                    "[DoubaoClient-Debug] 等待无水印原图，当前缓存: {}，拦截器状态: {:?}",
                    items.len(),
                    debug
                );
            }
        }

        Ok(None)
    }

    async fn wait_for_element(
        &self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<chromiumoxide::Element> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;

        if timeout_ms == 0 {
            // Wait indefinitely
            loop {
                if let Ok(elem) = page.find_element(selector).await {
                    return Ok(elem);
                }
                sleep(Duration::from_millis(500)).await;
            }
        }

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if let Ok(elem) = page.find_element(selector).await {
                return Ok(elem);
            }
            sleep(Duration::from_millis(500)).await;
        }

        Err(anyhow!("Timeout waiting for element: {selector}"))
    }
}
