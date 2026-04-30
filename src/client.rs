use anyhow::{anyhow, Result};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::network::{
    EventResponseReceived, GetResponseBodyParams,
};
use chromiumoxide::handler::viewport::Viewport;
use chromiumoxide::Page;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures::StreamExt;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::fs;
use tokio::time::sleep;

const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36";
const VIEWPORT_W: u32 = 1280;
const VIEWPORT_H: u32 = 800;

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

        let viewport = Viewport {
            width: VIEWPORT_W,
            height: VIEWPORT_H,
            device_scale_factor: None,
            emulating_mobile: false,
            is_landscape: true,
            has_touch: false,
        };

        let mut config_builder = BrowserConfig::builder()
            .viewport(viewport)
            .user_data_dir(self.user_data_dir.clone())
            .args(vec![
                format!("--user-agent={}", USER_AGENT),
                "--disable-blink-features=AutomationControlled".to_string(),
                "--disable-infobars".to_string(),
            ]);
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

        let page = Arc::new(browser.new_page("https://www.doubao.com/chat/").await?);
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
            .evaluate(r#"
                Array.from(document.querySelectorAll('button, a, div, span'))
                    .some(el => el.textContent.includes('登录') && el.offsetParent !== null)
            "#)
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

    pub async fn generate_image(
        &mut self,
        prompt: &str,
        quality: &str,
        ratio: Option<&str>,
        timeout_ms: u64,
    ) -> Result<Option<String>> {
        let page = self.page.as_ref().ok_or_else(|| anyhow!("Not initialized"))?;
        let final_prompt = match ratio {
            Some(r) => format!("{prompt}，图片比例 {r}"),
            None => prompt.to_string(),
        };

        println!("[DoubaoClient] 发送生图请求: {final_prompt} (质量: {quality})");

        // Clear previous intercepts
        self.intercepted_buffers.clear();

        // Start network interception
        let page_arc = Arc::clone(page);
        let intercept_task = tokio::spawn(async move {
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
                if url.contains("flow-imagex-sign") && url.contains("image_pre_watermark") {
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
        });

        // Find and fill textarea
        let textarea = self.wait_for_element("textarea", 10000).await?;
        textarea.click().await?;
        sleep(Duration::from_millis(200)).await;

        // Insert text via CDP Input.insertText (triggers React onChange, send button appears)
        let fill_text = format!("帮我生成图片：{final_prompt}");
        page.execute(
            chromiumoxide::cdp::browser_protocol::input::InsertTextParams::new(&fill_text)
        ).await?;
        sleep(Duration::from_millis(500)).await;

        // Count existing images
        let before_count: i32 = page
            .evaluate(r#"document.querySelectorAll('img[src*="flow-imagex-sign"]').length"#)
            .await?
            .into_value()?;
        println!("[DoubaoClient-Debug] 发送指令前，已有图片数量: {before_count}");

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
        ).await?;
        page.execute(
            DispatchKeyEventParams::builder()
                .r#type(DispatchKeyEventType::KeyUp)
                .key("Enter")
                .code("Enter")
                .windows_virtual_key_code(13)
                .native_virtual_key_code(13)
                .build()
                .unwrap(),
        ).await?;
        println!("[DoubaoClient] 已发送指令，等待图片生成...");

        // Poll for new images
        let start = Instant::now();
        let mut target_url: Option<String> = None;
        let mut poll_count = 0;

        while start.elapsed() < Duration::from_millis(timeout_ms) {
            sleep(Duration::from_millis(2000)).await;
            poll_count += 1;

            let current_count: i32 = page
                .evaluate(r#"document.querySelectorAll('img[src*="flow-imagex-sign"]').length"#)
                .await?
                .into_value()?;
            println!("[DoubaoClient-Debug] 第 {poll_count} 次轮询, 当前图片数量: {current_count}");

            if current_count > before_count {
                let src: String = page
                    .evaluate(r#"
                        (function() {
                            const imgs = document.querySelectorAll('img[src*="flow-imagex-sign"]');
                            return imgs[imgs.length - 1].getAttribute('src');
                        })()
                    "#)
                    .await?
                    .into_value()?;
                sleep(Duration::from_millis(3000)).await;
                target_url = Some(src);
                println!("[DoubaoClient] 检测到新图片生成");
                break;
            }
        }

        let target_url = match target_url {
            Some(u) => u,
            None => {
                println!("[DoubaoClient] 等待图片超时");
                return Ok(None);
            }
        };

        // If preview only, return immediately
        if quality == "preview" {
            return Ok(Some(target_url));
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

        // 4. Collect intercepted buffers
        sleep(Duration::from_millis(1000)).await;
        let intercepted = match tokio::time::timeout(Duration::from_secs(2), intercept_task).await {
            Ok(Ok(bufs)) => bufs,
            _ => HashMap::new(),
        };
        self.intercepted_buffers = intercepted;

        if !self.intercepted_buffers.is_empty() {
            let (first_url, first_buf) = self.intercepted_buffers.iter().next().unwrap();
            let _ = first_url;
            println!("[DoubaoClient] 网络拦截捕获到原图 ({} bytes)", first_buf.len());
        }

        // 5. Close modal
        page.evaluate(r#"
            document.dispatchEvent(new KeyboardEvent('keydown', {
                key: 'Escape', code: 'Escape', keyCode: 27, bubbles: true
            }));
            true
        "#).await?;
        sleep(Duration::from_millis(500)).await;

        if let Some(url) = best_url {
            println!("[DoubaoClient] 最终原图 URL: {}...", &url[..url.len().min(80)]);
            return Ok(Some(url));
        }

        println!("[DoubaoClient] 未能获取原图，回退到缩略图");
        Ok(Some(target_url))
    }

    async fn click_save_button(&self) -> Result<bool> {
        let page = self.page.as_ref().ok_or_else(|| anyhow!("Not initialized"))?;

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
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Download failed: HTTP {}", resp.status()));
        }

        let data = resp.bytes().await?;
        fs::write(dest, &data).await?;
        println!("[DoubaoClient] 图片已保存至: {} ({} bytes)", dest.display(), data.len());
        Ok(dest.clone())
    }

    pub async fn close(&mut self) {
        if let Some(mut browser) = self.browser.take() {
            let _ = browser.close().await;
            println!("[DoubaoClient] 浏览器已关闭。");
        }
    }

    async fn wait_for_element(&self, selector: &str, timeout_ms: u64) -> Result<chromiumoxide::Element> {
        let page = self.page.as_ref().ok_or_else(|| anyhow!("Not initialized"))?;

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
