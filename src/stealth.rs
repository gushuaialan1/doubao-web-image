use chromiumoxide::handler::viewport::Viewport;
use rand::Rng;

/// 浏览器身份：UA 字符串与 Client Hints 版本号，全部来自 CDP `Browser.getVersion`
/// 的真实返回值（保证 UA / Client Hints / 真实引擎三者自洽）。
#[derive(Debug, Clone)]
pub struct BrowserIdentity {
    /// 完整 UA（HeadlessChrome 已替换为 Chrome）
    pub user_agent: String,
    /// 大版本号（如 "139"）
    pub major_version: String,
    /// 完整版本号（如 "139.0.7258.66"，不足 4 段补 .0）
    pub full_version: String,
}

/// 兜底身份：仅在 CDP `Browser.getVersion` 异常失败时使用（正常启动后几乎不会走到）。
pub fn fallback_identity() -> BrowserIdentity {
    BrowserIdentity {
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/135.0.0.0 Safari/537.36".to_string(),
        major_version: "135".to_string(),
        full_version: "135.0.0.0".to_string(),
    }
}

/// 从 CDP `Browser.getVersion` 的返回值构建身份。
///
/// - `product` 形如 "HeadlessChrome/139.0.7258.66" 或 "Chrome/139.0.7258.66"
/// - `user_agent` 是浏览器真实默认 UA；无头模式下含 "HeadlessChrome"，
///   替换为 "Chrome" 后对外呈现（真实引擎版本号原样保留）。
pub fn identity_from_version(product: &str, user_agent: &str) -> BrowserIdentity {
    let raw_version = product.rsplit('/').next().unwrap_or(product).trim();
    let mut parts: Vec<&str> = raw_version.split('.').collect();
    while parts.len() < 4 {
        parts.push("0");
    }
    let full_version = parts[..4].join(".");
    let major_version = parts.first().unwrap_or(&"0").to_string();

    let ua = if user_agent.contains("HeadlessChrome") {
        user_agent.replace("HeadlessChrome", "Chrome")
    } else if !user_agent.contains("Chrome/") {
        // 极端情况：默认 UA 不含 Chrome token（理论上不会发生），按版本拼一个
        format!(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{full_version} Safari/537.36"
        )
    } else {
        user_agent.to_string()
    };

    BrowserIdentity {
        user_agent: ua,
        major_version,
        full_version,
    }
}

/// 补充 Stealth 脚本模板。
///
/// 在 chromiumoxide 内置 `enable_stealth_mode()` 基础上补充以下检测向量：
/// - navigator.webdriver → undefined（chromiumoxide 设为 false，覆盖为 undefined 更真实）
/// - navigator.languages / vendor / hardwareConcurrency / deviceMemory / maxTouchPoints
/// - navigator.userAgentData (Client Hints)——版本号占位符由 `build_stealth_script()`
///   用 CDP 拿到的真实浏览器版本替换，保证与 UA、真实引擎自洽
/// - screen 对象
/// - window.outerWidth/outerHeight
/// - Notification.permission
/// - Document.prototype.webdriver 清理
/// - Object.getOwnPropertyDescriptor 防护
/// - iframe navigator 继承
/// - Canvas 指纹噪声
/// - performance.now 单调性
const STEALTH_SCRIPT_TEMPLATE: &str = r#"
(() => {
    'use strict';

    // ===== Helper: Native Function Masking =====
    const nativeFns = new Set();
    const origToString = Function.prototype.toString;
    Function.prototype.toString = function() {
        if (nativeFns.has(this)) {
            return `function ${this.name || ''}() { [native code] }`;
        }
        return origToString.call(this);
    };
    const markNative = (fn) => {
        if (typeof fn === 'function') nativeFns.add(fn);
        return fn;
    };

    // ===== 1. Override webdriver (chromiumoxide sets false, we want undefined) =====
    Object.defineProperty(Object.getPrototypeOf(navigator), 'webdriver', {
        get: () => undefined,
        configurable: true,
        enumerable: true
    });

    // ===== 2. Languages =====
    Object.defineProperty(navigator, 'languages', {
        get: () => ['zh-CN', 'zh', 'en-US', 'en'],
        configurable: true,
        enumerable: true
    });

    // ===== 3. Vendor =====
    Object.defineProperty(navigator, 'vendor', {
        get: () => 'Google Inc.',
        configurable: true,
        enumerable: true
    });

    // ===== 4. Hardware Concurrency =====
    Object.defineProperty(navigator, 'hardwareConcurrency', {
        get: () => 8,
        configurable: true,
        enumerable: true
    });

    // ===== 5. Device Memory =====
    Object.defineProperty(navigator, 'deviceMemory', {
        get: () => 8,
        configurable: true,
        enumerable: true
    });

    // ===== 6. Max Touch Points =====
    Object.defineProperty(navigator, 'maxTouchPoints', {
        get: () => 0,
        configurable: true,
        enumerable: true
    });

    // ===== 7. User Agent Data (Client Hints) =====
    // 版本号由 Rust 侧用 CDP Browser.getVersion 的真实版本替换（__CHROME_MAJOR__ /
    // __CHROME_FULL_VERSION__），brand 组合与同期真实桌面 Chrome 一致
    // （Not/A)Brand v=8 + Chromium + Google Chrome）。
    Object.defineProperty(navigator, 'userAgentData', {
        get: () => ({
            brands: [
                { brand: 'Not/A)Brand', version: '8' },
                { brand: 'Chromium', version: '__CHROME_MAJOR__' },
                { brand: 'Google Chrome', version: '__CHROME_MAJOR__' }
            ],
            mobile: false,
            platform: 'Windows',
            getHighEntropyValues: markNative(function(hints) {
                return Promise.resolve({
                    architecture: 'x86',
                    bitness: '64',
                    model: '',
                    platformVersion: '19.0.0',
                    uaFullVersion: '__CHROME_FULL_VERSION__',
                    fullVersionList: [
                        { brand: 'Not/A)Brand', version: '8.0.0.0' },
                        { brand: 'Chromium', version: '__CHROME_FULL_VERSION__' },
                        { brand: 'Google Chrome', version: '__CHROME_FULL_VERSION__' }
                    ]
                });
            }),
            toJSON: markNative(function() {
                return { brands: this.brands, mobile: this.mobile, platform: this.platform };
            })
        }),
        configurable: true,
        enumerable: true
    });

    // ===== 8. Screen =====
    const screenObj = {
        width: 1920,
        height: 1080,
        availWidth: 1920,
        availHeight: 1040,
        availLeft: 0,
        availTop: 0,
        colorDepth: 24,
        pixelDepth: 24,
        orientation: { angle: 0, type: 'landscape-primary' }
    };
    Object.defineProperty(window, 'screen', {
        get: () => screenObj,
        configurable: true
    });

    // ===== 9. Outer dimensions =====
    Object.defineProperty(window, 'outerWidth', {
        get: () => window.innerWidth + 16,
        configurable: true
    });
    Object.defineProperty(window, 'outerHeight', {
        get: () => window.innerHeight + 133,
        configurable: true
    });

    // ===== 10. Notification.permission =====
    try {
        Object.defineProperty(Notification, 'permission', {
            get: () => 'default',
            configurable: true
        });
    } catch(e) {}

    // ===== 11. Document.prototype.webdriver cleanup =====
    try {
        delete Document.prototype.webdriver;
    } catch(e) {}

    // ===== 12. Prevent getOwnPropertyDescriptor detection =====
    const origGetOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
    Object.getOwnPropertyDescriptor = function(obj, prop) {
        if (obj === navigator && prop === 'webdriver') return undefined;
        return origGetOwnPropertyDescriptor.call(this, obj, prop);
    };

    // ===== 13. Iframe inheritance =====
    const origCreateElement = Document.prototype.createElement;
    Document.prototype.createElement = function(tagName, options) {
        const elem = origCreateElement.call(this, tagName, options);
        if (String(tagName).toLowerCase() === 'iframe') {
            try {
                const win = elem.contentWindow;
                if (win && win.navigator) {
                    Object.defineProperty(win.navigator, 'webdriver', {
                        get: () => undefined,
                        configurable: true,
                        enumerable: true
                    });
                }
            } catch(e) {}
        }
        return elem;
    };

    // ===== 14. Canvas fingerprint noise =====
    const origGetImageData = CanvasRenderingContext2D.prototype.getImageData;
    CanvasRenderingContext2D.prototype.getImageData = function(x, y, w, h) {
        const data = origGetImageData.call(this, x, y, w, h);
        // Imperceptible noise to first few pixels
        for (let i = 0; i < Math.min(data.data.length, 16); i += 4) {
            data.data[i] = (data.data[i] + 1) % 256;
        }
        return data;
    };

    // ===== 15. Performance.now monotonicity =====
    const origNow = performance.now.bind(performance);
    let lastNow = 0;
    performance.now = function() {
        const n = origNow();
        if (n < lastNow) return lastNow;
        lastNow = n;
        return n;
    };

})();
"#;

/// 用真实浏览器身份实例化 stealth 脚本（替换 Client Hints 版本号占位符）。
pub fn build_stealth_script(identity: &BrowserIdentity) -> String {
    STEALTH_SCRIPT_TEMPLATE
        .replace("__CHROME_MAJOR__", &identity.major_version)
        .replace("__CHROME_FULL_VERSION__", &identity.full_version)
}

/// 构建增强的 Chrome 启动参数。
///
/// 参考 puppeteer-extra-stealth 的启动参数列表，移除或禁用可能暴露自动化特征的功能。
/// 不设置 `--user-agent=`：启动前无法知道自动下载的浏览器真实版本，硬编码会与真实
/// 引擎不一致；改为启动后经 CDP `Browser.getVersion` 取真实版本，再逐页用
/// `Emulation.setUserAgentOverride` 覆盖（所有目标页面导航都发生在 override 之后）。
/// 同时移除了 `--disable-gpu` / `--disable-accelerated-2d-canvas`——真实桌面 Chrome
/// 不会带这两个 flag，是无头自动化的典型自曝向量。
pub fn build_stealth_args() -> Vec<String> {
    vec![
        "--disable-blink-features=AutomationControlled".to_string(),
        "--disable-infobars".to_string(),
        "--disable-web-security".to_string(),
        "--disable-features=IsolateOrigins,site-per-process,InterestFeedContentSuggestions,OptimizationHints,NetworkPrediction,Translate,HandwritingPredictionUI,IdleDetection,InterestCohort".to_string(),
        "--disable-site-isolation-trials".to_string(),
        "--disable-dev-shm-usage".to_string(),
        "--no-sandbox".to_string(),
        "--disable-setuid-sandbox".to_string(),
        "--hide-scrollbars".to_string(),
        "--disable-notifications".to_string(),
        "--disable-background-timer-throttling".to_string(),
        "--disable-backgrounding-occluded-windows".to_string(),
        "--disable-breakpad".to_string(),
        "--disable-component-update".to_string(),
        "--disable-default-apps".to_string(),
        "--disable-hang-monitor".to_string(),
        "--disable-ipc-flooding-protection".to_string(),
        "--disable-popup-blocking".to_string(),
        "--disable-prompt-on-repost".to_string(),
        "--disable-renderer-backgrounding".to_string(),
        "--force-color-profile=srgb".to_string(),
        "--metrics-recording-only".to_string(),
        "--no-first-run".to_string(),
        "--password-store=basic".to_string(),
        "--use-mock-keychain".to_string(),
        "--enable-features=NetworkService,NetworkServiceInProcess".to_string(),
        "--window-position=0,0".to_string(),
    ]
}

/// 生成随机化但合理的 viewport。
///
/// 在常见桌面分辨率附近小幅波动，避免所有请求使用完全相同的尺寸。
pub fn random_viewport() -> Viewport {
    let mut rng = rand::thread_rng();

    let widths = [1280, 1366, 1440, 1536, 1600, 1920];
    let heights = [720, 768, 800, 864, 900, 1080];

    let w = widths[rng.gen_range(0..widths.len())];
    let h = heights[rng.gen_range(0..heights.len())];

    Viewport {
        width: w,
        height: h,
        device_scale_factor: Some(1.0),
        emulating_mobile: false,
        is_landscape: true,
        has_touch: false,
    }
}
