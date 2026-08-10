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
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
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

/// --direct 直出收图结果。
#[derive(Debug)]
pub struct DirectCollection {
    /// 已保存的图片（按收到顺序）；bool 表示是否为 SSE 拦截的无水印原图
    /// （false 的需要由调用方按需做本地去水印处理）。
    pub images: Vec<(PathBuf, bool)>,
    /// 累积抓取的 AI 文字回复（所有轮次拼接）。
    pub reply_text: String,
    /// 实际发送的催更次数。
    pub continues: u32,
}

pub struct DoubaoClient {
    browser: Option<Browser>,
    page: Option<Arc<Page>>,
    user_data_dir: PathBuf,
    intercepted_buffers: HashMap<String, Vec<u8>>,
    /// 本轮回合中拦截到的所有图片响应 URL（无论响应体是否成功读取）。
    /// GetResponseBody 存在竞态（-32000 No data found），体丢了 URL 仍可走 reqwest 回退下载。
    intercepted_urls: Vec<String>,
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
            intercepted_urls: Vec::new(),
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

        // 浏览器启动加超时 + 明确错误：同一 session profile 被另一个 CLI 实例占用
        // （或有残留 chrome 进程）时，launch 可能挂起或 chrome 启动即退出
        // （exit status 21），给出可操作的提示而不是干等/裸错误。
        let launch_result =
            tokio::time::timeout(Duration::from_secs(45), Browser::launch(config)).await;
        let (browser, mut handler) = match launch_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                let hint = if self.user_data_dir.join("DevToolsActivePort").exists() {
                    format!(
                        "检测到 {} 下存在 DevToolsActivePort，很可能有残留 chrome 进程仍占用该 session profile；请结束命令行包含 .doubao-web-session 的 chrome.exe 后重试。",
                        self.user_data_dir.display()
                    )
                } else {
                    String::new()
                };
                return Err(anyhow!("浏览器启动失败: {e}。{hint}"));
            }
            Err(_) => {
                return Err(anyhow!(
                    "浏览器启动超时（45s）。session 目录 {} 可能被另一个 doubao-web-image 实例占用，或存在残留的 chrome 进程；请关闭后重试。",
                    self.user_data_dir.display()
                ));
            }
        };

        // Spawn browser event handler
        tokio::spawn(async move {
            while let Some(h) = handler.next().await {
                if h.is_err() {
                    break;
                }
            }
        });

        // launch 成功后的任何失败都要先关闭浏览器进程再返回，避免残留进程占用 profile
        let (page, url) = match Self::prepare_page(&browser).await {
            Ok(pair) => pair,
            Err(e) => {
                let mut browser = browser;
                let _ = browser.close().await;
                return Err(e);
            }
        };

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

    /// 创建页面、注入 stealth / SSE 拦截脚本并导航到豆包首页。
    /// 从 init 拆出：调用方在本函数失败时先关闭浏览器进程再返回错误，避免残留。
    async fn prepare_page(browser: &Browser) -> Result<(Arc<Page>, String)> {
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

        Ok((page, url))
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

        self.generate_with_message(
            &format!("帮我生成图片：{final_prompt}"),
            references,
            quality,
            timeout_ms,
        )
        .await
    }

    /// 首轮取图失败后的补救：在同一对话里补发一条「按刚才的要求重新生成」，
    /// 再按与首轮完全相同的逻辑等一次新图。单图/批量模式共用。
    pub async fn regenerate_and_wait(
        &mut self,
        quality: &str,
        timeout_ms: u64,
    ) -> Result<Option<GeneratedImageInfo>> {
        println!("[DoubaoClient] 在同一对话补发「重新生成」请求...");
        self.generate_with_message(
            "按刚才的要求重新生成一张，保持同样的内容、风格和比例",
            &[],
            quality,
            timeout_ms,
        )
        .await
    }

    /// 发送一条生图消息并等待新图（generate_image / regenerate_and_wait 的共用实现）。
    async fn generate_with_message(
        &mut self,
        fill_text: &str,
        references: &[PathBuf],
        quality: &str,
        timeout_ms: u64,
    ) -> Result<Option<GeneratedImageInfo>> {
        // Clear previous intercepts
        self.intercepted_buffers.clear();
        self.intercepted_urls.clear();

        // Start network interception (captures original image responses for this turn)
        let mut intercept_task = self.spawn_response_interceptor()?;

        // 保险：SPA 整页导航后 init 时注入的 SSE hook 会随文档销毁而丢失，
        // 发送前补注入一次（脚本内部有幂等 guard，重复执行无副作用）。
        self.ensure_stream_interceptor().await;

        // 清场：关闭上一轮可能残留的大图 viewer/弹层，避免遮挡输入区或劫持焦点
        // （补发重试、批量连续发图时尤其重要）。
        if let Some(page) = self.page.as_ref() {
            let _ = page
                .evaluate(
                    r#"
                    document.dispatchEvent(new KeyboardEvent('keydown', {
                        key: 'Escape', code: 'Escape', keyCode: 27, bubbles: true
                    }));
                    true
                "#,
                )
                .await;
        }
        sleep(Duration::from_millis(500)).await;

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
        self.send_message(fill_text, references).await?;
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

    /// 确保 SSE 拦截脚本已注入当前文档（脚本内部有幂等 guard）。
    async fn ensure_stream_interceptor(&self) {
        if let Some(page) = self.page.as_ref() {
            let _ = page.evaluate(STREAM_INTERCEPTOR_SCRIPT).await;
        }
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

        // Find and fill textarea (acquire after upload: React may re-render the input area).
        // 优先用 JS focus/click（对 viewer 残留遮盖、React 重渲染导致的元素句柄失效
        // 免疫——Element::click 在元素无可见区域时会报 No value found），失败再退回元素点击。
        let focused: bool = page
            .evaluate(
                r#"
                (function() {
                    const ta = document.querySelector('textarea');
                    if (!ta) return false;
                    ta.focus();
                    ta.click();
                    return true;
                })()
            "#,
            )
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or(false);
        if !focused {
            let textarea = self.wait_for_element("textarea", 10000).await?;
            textarea.click().await?;
        }
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
    /// 返回 (URL→响应体, 全部匹配 URL 列表)。GetResponseBody 存在竞态
    /// （Error -32000 No data found，响应体被页面消费/回收），读体失败时 URL
    /// 仍保留在列表里，供模态框提取失败时走 reqwest 回退下载。
    /// 返回的 JoinHandle 由调用方在等待结束后收集并 abort，避免批量模式下任务堆积。
    #[allow(clippy::type_complexity)]
    fn spawn_response_interceptor(
        &self,
    ) -> Result<tokio::task::JoinHandle<(HashMap<String, Vec<u8>>, Vec<String>)>> {
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
                    return (HashMap::<String, Vec<u8>>::new(), Vec::new());
                }
            };

            let mut buffers = HashMap::new();
            let mut urls: Vec<String> = Vec::new();
            while let Some(event) = events.next().await {
                let url = &event.response.url;
                if url.contains("flow-imagex-sign") || url.contains("image_pre_watermark") {
                    // 无论响应体能否读到，都先记录 URL（读体竞态失败时的回退下载来源）
                    if !urls.contains(url) {
                        urls.push(url.clone());
                    }
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
                            eprintln!(
                                "[DoubaoClient] Failed to get response body: {e}（已保留 URL 供回退下载）"
                            );
                        }
                    }
                }
            }
            (buffers, urls)
        }))
    }

    /// 等待当前对话中出现「发送前快照之后新增」的图片，并按 quality 提取最优 URL。
    ///
    /// before_urls / before_ori_count 由调用方在发送消息前快照，同一对话里已有的
    /// 旧图不会被误判为新图（批量模式的关键）。如果豆包一次回复多张候选图，
    /// 沿用既有策略取最新（最后）一张。
    ///
    /// 取图为双通道竞速：SSE 拦截的 image_ori_raw 无水印原图（优先）与 DOM 新图
    /// diff 每轮都查，哪一路先出结果就走哪一路——豆包有时不走页面 fetch/XHR 发起
    /// /chat/completion（疑似 Web Worker 或灰度传输），SSE 通道会整体失效，此时
    /// DOM 通道必须能立即接管，而不是等满 SSE 超时才回退。轮询中的单次 CDP 调用
    /// 失败（页面卡顿导致的 Request timed out）只跳过本轮，不再直接判死。
    async fn wait_for_new_image(
        &mut self,
        intercept_task: &mut tokio::task::JoinHandle<(HashMap<String, Vec<u8>>, Vec<String>)>,
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

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut poll_count = 0;
        let mut target_url: Option<String> = None;

        while Instant::now() < deadline {
            sleep(Duration::from_millis(1500)).await;
            poll_count += 1;

            // 通道 1：SSE 拦截的 image_ori_raw 无水印原图（优先，可跳过模态框提取）
            if quality != "preview" {
                match self.ori_raw_list().await {
                    Ok(items) if items.len() > before_ori_count => {
                        let item = items.last().unwrap();
                        println!(
                            "[DoubaoClient] SSE 拦截到无水印原图 ({}x{}): {}...",
                            item.width,
                            item.height,
                            &item.url[..item.url.len().min(60)]
                        );
                        intercept_task.abort();
                        return Ok(Some(GeneratedImageInfo {
                            url: item.url.clone(),
                            is_watermark_free: true,
                        }));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!("[DoubaoClient-Debug] 轮询 SSE 缓存失败（跳过本轮）: {e}")
                    }
                }
            }

            // 通道 2：DOM 新图 diff（by URL diff, not just count）
            match self.current_image_urls().await {
                Ok(current_urls) => {
                    if poll_count % 4 == 0 {
                        println!(
                            "[DoubaoClient-Debug] 第 {poll_count} 次轮询, 当前图片数量: {}",
                            current_urls.len()
                        );
                    }
                    let new_urls: Vec<String> = current_urls
                        .iter()
                        .filter(|url| !before_urls.contains(url))
                        .cloned()
                        .collect();
                    if !new_urls.is_empty() {
                        // Take the last new URL (most likely the newest generated image)
                        target_url = Some(new_urls.last().unwrap().clone());
                        println!(
                            "[DoubaoClient] 检测到新图片生成 (新增 {} 张)",
                            new_urls.len()
                        );
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("[DoubaoClient-Debug] 轮询 DOM 图片失败（跳过本轮）: {e}")
                }
            }

            // 周期性输出 SSE 拦截器状态，便于诊断「SSE 通道整体失效」类问题
            if poll_count % 8 == 0 && quality != "preview" {
                let cache = self.ori_raw_count().await.unwrap_or(0);
                let debug: InterceptorDebug = page
                    .evaluate("window.__doubaoInterceptorDebug || {}")
                    .await
                    .ok()
                    .and_then(|v| v.into_value().ok())
                    .unwrap_or_default();
                println!(
                    "[DoubaoClient-Debug] 等待新图，SSE 缓存: {cache}，拦截器状态: {debug:?}"
                );
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

        // 等缩略图加载稳定（与原实现一致的 3s 停顿）
        sleep(Duration::from_millis(3000)).await;

        // DOM 先出图：ori_raw 通常紧随其后，给 SSE 一个 8s 短宽限，
        // 拿到无水印原图就免走模态框；拿不到再进模态框提取。
        if quality != "preview" {
            if let Ok(Some(item)) = self.poll_new_ori_raw(before_ori_count, 8_000).await {
                println!("[DoubaoClient] 宽限期内 SSE 拦截到无水印原图，跳过模态框提取");
                intercept_task.abort();
                return Ok(Some(GeneratedImageInfo {
                    url: item.url,
                    is_watermark_free: true,
                }));
            }
        }

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

        // 0. 点击前重新 diff 一次拿到最新目标缩略图（生成过程中占位图/预览可能被
        //    React 替换，早先捕获的 src 已变化），并提取 imagex 对象 key 用于稳定匹配。
        let mut target_url = target_url;
        if let Ok(current_urls) = self.current_image_urls().await {
            let new_urls: Vec<String> = current_urls
                .iter()
                .filter(|url| !before_urls.contains(url))
                .cloned()
                .collect();
            if let Some(newest) = new_urls.last() {
                target_url = newest.clone();
            }
        }
        let target_key = Self::extract_imagex_key(&target_url);
        println!(
            "[DoubaoClient-Debug] 目标缩略图: {}...（key: {target_key}）",
            &target_url[..target_url.len().min(60)]
        );

        // 1. Click thumbnail to open modal（按对象 key 匹配点击目标。
        //    旧实现用 URL 前 30 字符匹配，那只是 CDN 主机名前缀，host 不同就
        //    静默点空、模态框根本没打开，最后退化成下载内联小预览图。）
        let click_script = format!(
            r#"
            (function() {{
                const key = '{}';
                const imgs = document.querySelectorAll('img[src*="flow-imagex-sign"]');
                if (key) {{
                    for (let i = imgs.length - 1; i >= 0; i--) {{
                        const src = imgs[i].getAttribute('src') || '';
                        if (src.includes(key)) {{
                            imgs[i].click();
                            return 'key';
                        }}
                    }}
                }}
                if (imgs.length) {{
                    imgs[imgs.length - 1].click();
                    return 'last';
                }}
                return '';
            }})()
            "#,
            target_key
        );
        let click_way: String = page
            .evaluate(click_script.as_str())
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or_default();
        println!("[DoubaoClient-Debug] 模态框点击方式: {}", if click_way.is_empty() { "未命中".to_string() } else { click_way });

        // 等大图加载。当前豆包大图 URL 已无 image_pre_watermark 之类的固定标记，
        // 无法再从 DOM 判定模态框是否打开；点击后大图请求必然产生大响应体
        // （内联预览均 <100KB），后续靠网络拦截的大响应体识别。
        sleep(Duration::from_millis(3000)).await;

        // 2. Click save button（触发原图请求，供网络拦截捕获响应体；失败不判死）
        let clicked = self.click_save_button().await.unwrap_or(false);
        if clicked {
            println!("[DoubaoClient] 已点击保存按钮");
        } else {
            println!("[DoubaoClient] 未找到保存按钮，直接提取 URL");
        }

        sleep(Duration::from_millis(2000)).await;

        // 3. Extract original URL from DOM（pre_watermark 大图优先——老版豆包标记；
        //    其次只接受与目标同对象 key 的非压缩图，避免抓到内联小预览/侧栏缩略图。
        //    单次 CDP 异常不判死，按未提取到处理）
        let extract_script = format!(
            r#"
            (function() {{
                const key = '{}';
                const imgs = Array.from(document.querySelectorAll('img[src*="flow-imagex-sign"]'));
                for (const img of imgs) {{
                    const src = img.getAttribute('src');
                    if (src && src.includes('image_pre_watermark')) {{
                        return src;
                    }}
                }}
                if (key) {{
                    for (const img of imgs) {{
                        const src = img.getAttribute('src');
                        if (src && src.includes(key) && !src.includes('downsize') && !src.includes('web-operation') && !src.includes('avatar')) {{
                            return src;
                        }}
                    }}
                }}
                return null;
            }})()
            "#,
            target_key
        );
        let dom_best_url: Option<String> = page
            .evaluate(extract_script.as_str())
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .flatten();

        // 4. Collect intercepted buffers + URL list, then stop the interceptor for this turn
        sleep(Duration::from_millis(1000)).await;
        let (intercepted, intercepted_urls) =
            match tokio::time::timeout(Duration::from_secs(2), &mut *intercept_task).await {
                Ok(Ok(pair)) => pair,
                _ => (HashMap::new(), Vec::new()),
            };
        intercept_task.abort();
        self.intercepted_buffers = intercepted;
        self.intercepted_urls = intercepted_urls;

        if !self.intercepted_buffers.is_empty() {
            let (first_url, first_buf) = self.intercepted_buffers.iter().next().unwrap();
            let _ = first_url;
            println!(
                "[DoubaoClient] 网络拦截捕获到原图 ({} bytes)",
                first_buf.len()
            );
        }

        // 5. Close modal（结果忽略，页面异常不判死）
        let _ = page
            .evaluate(
                r#"
            document.dispatchEvent(new KeyboardEvent('keydown', {
                key: 'Escape', code: 'Escape', keyCode: 27, bubbles: true
            }));
            true
        "#,
            )
            .await;
        sleep(Duration::from_millis(500)).await;

        // 末轮复查 SSE 缓存：生成完成较晚时 ori_raw 可能在模态框阶段才到达
        // （实测宽限期结束后、模态框点击期间缓存从 0 涨到 4），无水印原图优先。
        if quality != "preview" {
            if let Ok(items) = self.ori_raw_list().await {
                if items.len() > before_ori_count {
                    let item = items.last().unwrap();
                    println!("[DoubaoClient] 末轮复查 SSE 拦截到无水印原图，采用之");
                    return Ok(Some(GeneratedImageInfo {
                        url: item.url.clone(),
                        is_watermark_free: true,
                    }));
                }
            }
        }

        // 提取链：DOM → 拦截缓冲区大响应体（模态框大图，体已在内存可直接落盘）
        //        → 拦截 URL 列表（体竞态丢失时 reqwest 回退下载）
        let best_url = dom_best_url
            .or_else(|| {
                self.intercepted_urls
                    .iter()
                    .find(|u| {
                        self.intercepted_buffers
                            .get(*u)
                            .map(|b| b.len() > 100_000)
                            .unwrap_or(false)
                    })
                    .map(|u| {
                        println!("[DoubaoClient] 采用拦截缓冲区中的大图响应体");
                        u.clone()
                    })
            })
            .or_else(|| {
                let fallback = self
                    .intercepted_urls
                    .iter()
                    .rev()
                    .find(|u| u.contains("image_pre_watermark"))
                    .or_else(|| {
                        self.intercepted_urls.iter().rev().find(|u| {
                            !u.contains("downsize")
                                && !u.contains("web-operation")
                                && !u.contains("avatar")
                        })
                    })
                    .cloned();
                if let Some(u) = &fallback {
                    println!(
                        "[DoubaoClient] DOM 未提取到原图，使用网络拦截 URL 兜底: {}...",
                        &u[..u.len().min(80)]
                    );
                }
                fallback
            });

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

        // original 质量下只拿到内联小预览（豆包候选预览是 ~320px 小对象）不算成功：
        // 返回 None 让上层触发同对话补发重试，而不是静默交付低清图。
        println!("[DoubaoClient] 未能获取原图级 URL（仅剩内联小预览），本轮判失败");
        Ok(None)
    }

    /// 从 imagex 签名 URL 中提取对象 key（`.../<key>.<ext>~tplv-...` 的 key 部分）。
    /// 同一图片的 downsize/qvalue/raw 等模板变体共享同一 key，可跨变体稳定匹配。
    fn extract_imagex_key(url: &str) -> String {
        let path = url.split(['?', '~']).next().unwrap_or(url);
        let file = path.rsplit('/').next().unwrap_or(path);
        file.split('.').next().unwrap_or(file).to_string()
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
            let found: bool = page
                .evaluate(script.as_str())
                .await
                .ok()
                .and_then(|v| v.into_value().ok())
                .unwrap_or(false);
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
            let found: bool = page
                .evaluate(script.as_str())
                .await
                .ok()
                .and_then(|v| v.into_value().ok())
                .unwrap_or(false);
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
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
            .unwrap_or(false);

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

        // 签名 URL 偶发连接中断 / 响应体解码失败（error decoding response body），
        // 做有限重试（最多 3 次，递增退避）。
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=3u32 {
            if attempt > 1 {
                println!("[DoubaoClient] 下载重试第 {attempt}/3 次...");
                sleep(Duration::from_millis(1500 * u64::from(attempt))).await;
            }
            match Self::try_download(url).await {
                Ok(data) => {
                    fs::write(dest, &data).await?;
                    println!(
                        "[DoubaoClient] 图片已保存至: {} ({} bytes)",
                        dest.display(),
                        data.len()
                    );
                    return Ok(dest.clone());
                }
                Err(e) => {
                    eprintln!("[DoubaoClient] 下载失败（第 {attempt}/3 次）: {e}");
                    last_err = Some(e);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("Download failed")))
    }

    /// 单次下载尝试：返回完整响应体字节。
    async fn try_download(url: &str) -> Result<Vec<u8>> {
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
        Ok(data.to_vec())
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

    /// 非阻塞读取当前 SSE 拦截到的无水印原图列表（双通道竞速轮询用）。
    async fn ori_raw_list(&self) -> Result<Vec<ImageOriRawItem>> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("Not initialized"))?;
        let items: Vec<ImageOriRawItem> = page
            .evaluate("window.__doubaoImageOriRaws || []")
            .await?
            .into_value()?;
        Ok(items)
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

    /// 抓取当前对话中所有 AI 回复气泡的文字（按文档顺序拼接去重）。
    /// 主选择器是豆包 markdown 渲染根的语义类 `md-box-root`（经探针核实；
    /// 哈希类名 container-XXXX 会随部署变化，不作锚点），排除思维链盒子与
    /// 用户发送气泡；老的选择器层叠作为兜底。单次 CDP 异常返回 None（跳过本轮）。
    async fn collect_ai_reply_text(&self) -> Option<String> {
        let page = self.page.as_ref()?;
        let text: String = page
            .evaluate(
                r#"
                (function() {
                    const collect = (els, skipNestedSel) => {
                        const seen = new Set();
                        const out = [];
                        for (const el of els) {
                            // 跳过嵌套在已匹配元素内部的重复容器，避免文本重复
                            if (skipNestedSel && el.parentElement && el.parentElement.closest(skipNestedSel)) continue;
                            const t = (el.innerText || '').trim();
                            if (!t || seen.has(t)) continue;
                            seen.add(t);
                            out.push(t);
                        }
                        return out;
                    };
                    // 主路径：AI 回复的 markdown 渲染根（排除思维链与用户发送气泡）
                    const mdRoots = Array.from(document.querySelectorAll('div.md-box-root'))
                        .filter(el => !el.closest('[class*="thinking-box"], [class*="send-msg-bubble"]'));
                    const parts = collect(mdRoots, 'div.md-box-root');
                    if (parts.length) return parts.join('\n');
                    // 兜底：老选择器层叠
                    for (const sel of [
                        '[class*="markdown-body"]',
                        '[class*="markdown"]',
                        '[class*="receive-message"]',
                        '[data-testid*="message"]'
                    ]) {
                        const fallback = collect(Array.from(document.querySelectorAll(sel)), sel);
                        if (fallback.length) return fallback.join('\n');
                    }
                    return '';
                })()
            "#,
            )
            .await
            .ok()
            .and_then(|v| v.into_value().ok())?;
        Some(text)
    }

    /// --direct 直出收图：整篇消息发出后进入收图循环，把对话里陆续生成的图全部按序收下。
    ///
    /// 收图逻辑复用加固后的双通道件（DOM 新图 diff + SSE image_ori_raw 缓存）：
    /// DOM diff 按出现顺序发现新图（以 imagex 对象 key 去重，模板替换不会重复计数）；
    /// original 质量下与 SSE 无水印原图按到达顺序一一配对下载，preview 质量直接下 DOM 图。
    /// 一轮回复结束（25s 无新图/新文字）且未达 max_images 时发送 continue_prompt 催更
    /// （最多 max_continues 次）；最后一次活动后 settle_seconds 无进展、达到 max_images
    /// 或超过 overall_timeout_ms 时结束。收尾时未配对的图按 DOM 预览降级下载并明确标注。
    #[allow(clippy::too_many_arguments)]
    pub async fn run_direct_collection(
        &mut self,
        message: &str,
        output_dir: &Path,
        max_images: usize,
        settle_seconds: u64,
        continue_prompt: &str,
        max_continues: u32,
        quality: &str,
        overall_timeout_ms: u64,
    ) -> Result<DirectCollection> {
        fs::create_dir_all(output_dir).await?;

        // 发送前准备：补注入 SSE hook、Escape 清场、快照已有图片
        self.ensure_stream_interceptor().await;
        if let Some(page) = self.page.as_ref() {
            let _ = page
                .evaluate(
                    r#"
                    document.dispatchEvent(new KeyboardEvent('keydown', {
                        key: 'Escape', code: 'Escape', keyCode: 27, bubbles: true
                    }));
                    true
                "#,
                )
                .await;
        }
        sleep(Duration::from_millis(500)).await;
        let before_urls = self.current_image_urls().await.unwrap_or_default();
        let before_ori_count = self.ori_raw_count().await.unwrap_or(0);
        // 以 imagex 对象 key 去重（同一图的 downsize/qvalue 等模板变体共享 key）
        let mut seen_keys: HashSet<String> = before_urls
            .iter()
            .map(|u| Self::extract_imagex_key(u))
            .collect();
        println!(
            "[DoubaoClient-Debug] 直出发送前，已有图片数量: {}，无水印原图缓存: {before_ori_count}",
            before_urls.len()
        );

        self.send_message(message, &[]).await?;
        println!(
            "[DoubaoClient] 直出消息已发送，进入收图循环（上限 {max_images} 张，settle {settle_seconds}s）..."
        );

        let deadline = Instant::now() + Duration::from_millis(overall_timeout_ms);
        let settle = Duration::from_secs(settle_seconds);
        let round_idle = Duration::from_secs(25).min(settle);
        let mut last_activity = Instant::now();
        let mut continues = 0u32;
        let mut images: Vec<(PathBuf, bool)> = Vec::new();
        let mut pending: VecDeque<String> = VecDeque::new();
        let mut next_ori = before_ori_count;
        let mut reply_text = String::new();
        let mut poll_count = 0u32;

        loop {
            sleep(Duration::from_millis(2000)).await;
            poll_count += 1;
            let now = Instant::now();
            if now >= deadline {
                println!(
                    "[DoubaoClient] 达到总时长上限（{}s），结束收图",
                    overall_timeout_ms / 1000
                );
                break;
            }

            // a. AI 文字回复：气泡在 DOM 中持久存在，取历史最长快照即为全部轮次拼接
            if let Some(text) = self.collect_ai_reply_text().await {
                if text.len() > reply_text.len() {
                    reply_text = text;
                    last_activity = now;
                }
            }

            // b. DOM 新图（按对象 key 去重）入队，保持出现顺序
            if let Ok(cur) = self.current_image_urls().await {
                for u in cur {
                    let key = Self::extract_imagex_key(&u);
                    if !key.is_empty() && !seen_keys.contains(&key) {
                        seen_keys.insert(key);
                        pending.push_back(u);
                        last_activity = now;
                        println!(
                            "[DoubaoClient] 发现新图（待收 {} 张）",
                            pending.len()
                        );
                    }
                }
            }

            // c. 收图：original 与 SSE 原图按序配对，preview 直接下 DOM 图
            if quality == "preview" {
                while let Some(u) = pending.pop_front() {
                    if images.len() >= max_images {
                        break;
                    }
                    let dest = output_dir.join(format!("img_{:02}.png", images.len()));
                    match Self::download_image(&u, &dest).await {
                        Ok(p) => {
                            println!("[DoubaoClient] 已收第 {} 张（对话预览图）", images.len() + 1);
                            images.push((p, false));
                            last_activity = now;
                        }
                        Err(e) => eprintln!("⚠️ 图片下载失败（跳过）: {e}"),
                    }
                }
            } else if let Ok(ori) = self.ori_raw_list().await {
                while !pending.is_empty() && ori.len() > next_ori && images.len() < max_images {
                    let item = ori[next_ori].clone();
                    next_ori += 1;
                    pending.pop_front();
                    let dest = output_dir.join(format!("img_{:02}.png", images.len()));
                    match Self::download_image(&item.url, &dest).await {
                        Ok(p) => {
                            println!(
                                "[DoubaoClient] 已收第 {} 张（SSE 无水印原图 {}x{}）",
                                images.len() + 1,
                                item.width,
                                item.height
                            );
                            images.push((p, true));
                            last_activity = now;
                        }
                        Err(e) => eprintln!("⚠️ 图片下载失败（跳过）: {e}"),
                    }
                }
            }

            if images.len() >= max_images {
                println!("[DoubaoClient] 已达 maxImages={max_images}，结束收图");
                break;
            }

            // d. 空闲处理：先到轮次空闲阈值则催更，到 settle 则结束
            let idle = now.duration_since(last_activity);
            if idle >= settle {
                println!("[DoubaoClient] {settle_seconds}s 无任何进展，判定收图结束");
                break;
            }
            if idle >= round_idle
                && continues < max_continues
                && !continue_prompt.trim().is_empty()
            {
                println!(
                    "[DoubaoClient] 一轮回复已结束（{}s 无进展），发送催更「{}」（第 {}/{max_continues} 次）",
                    idle.as_secs(),
                    continue_prompt,
                    continues + 1
                );
                match self.send_message(continue_prompt, &[]).await {
                    Ok(()) => {
                        continues += 1;
                        last_activity = Instant::now();
                        // 人性化间隔：催更后缓一缓再恢复轮询
                        sleep(Duration::from_millis(3000)).await;
                    }
                    Err(e) => eprintln!("⚠️ 催更发送失败: {e}"),
                }
            }

            if poll_count % 10 == 0 {
                println!(
                    "[DoubaoClient-Debug] 收图中：已存 {} 张，待配对 {} 张，催更 {continues} 次",
                    images.len(),
                    pending.len()
                );
            }
        }

        // 收尾：original 质量下，先把 SSE 里尚未消费的原图收完（DOM 因虚拟列表
        // 可能漏显），再对一直没等到原图的 pending 项降级下载对话预览。
        if quality != "preview" {
            if let Ok(ori) = self.ori_raw_list().await {
                while ori.len() > next_ori && images.len() < max_images {
                    let item = ori[next_ori].clone();
                    next_ori += 1;
                    pending.pop_front();
                    let dest = output_dir.join(format!("img_{:02}.png", images.len()));
                    match Self::download_image(&item.url, &dest).await {
                        Ok(p) => {
                            println!(
                                "[DoubaoClient] 收尾补收第 {} 张（SSE 无水印原图）",
                                images.len() + 1
                            );
                            images.push((p, true));
                        }
                        Err(e) => eprintln!("⚠️ 收尾下载失败（跳过）: {e}"),
                    }
                }
            }
        }
        while let Some(u) = pending.pop_front() {
            if images.len() >= max_images {
                break;
            }
            println!(
                "⚠️ 第 {} 张未等到无水印原图，降级下载对话内预览",
                images.len() + 1
            );
            let dest = output_dir.join(format!("img_{:02}.png", images.len()));
            match Self::download_image(&u, &dest).await {
                Ok(p) => images.push((p, false)),
                Err(e) => eprintln!("⚠️ 收尾下载失败（跳过）: {e}"),
            }
        }

        println!(
            "[DoubaoClient] 收图结束：共 {} 张，催更 {continues} 次，回复文字 {} 字",
            images.len(),
            reply_text.chars().count()
        );
        Ok(DirectCollection {
            images,
            reply_text,
            continues,
        })
    }

    /// 调试探针：在当前页面执行任意 JS 并返回 JSON 值（仅供 dom_probe 诊断用）。
    #[doc(hidden)]
    pub async fn debug_eval(&self, js: &str) -> Option<serde_json::Value> {
        let page = self.page.as_ref()?;
        page.evaluate(js)
            .await
            .ok()
            .and_then(|v| v.into_value().ok())
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
