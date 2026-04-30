import { chromium, BrowserContext, Page } from 'playwright';
import * as path from 'path';
import * as os from 'os';
import * as fs from 'fs';
import * as https from 'https';

export class DoubaoClient {
    private context: BrowserContext | null = null;
    private page: Page | null = null;
    private userDataDir: string;
    private interceptedBuffers: Map<string, Buffer> = new Map();
    private responseHandler: ((response: any) => void) | null = null;

    constructor() {
        this.userDataDir = path.join(os.homedir(), '.doubao-web-session');
        if (!fs.existsSync(this.userDataDir)) {
            fs.mkdirSync(this.userDataDir, { recursive: true });
        }
    }

    async init(headless: boolean = false) {
        console.log(`[DoubaoClient] Initializing Playwright (headless: ${headless})...`);
        console.log(`[DoubaoClient] User data directory: ${this.userDataDir}`);
        
        this.context = await chromium.launchPersistentContext(this.userDataDir, {
            headless,
            viewport: { width: 1280, height: 800 },
            userAgent: 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36',
            args: [
                '--disable-blink-features=AutomationControlled',
                '--disable-infobars'
            ]
        });

        const pages = this.context.pages();
        this.page = pages.length > 0 ? pages[0] : (await this.context.newPage());

        console.log('[DoubaoClient] Navigating to Doubao chat...');
        if (!this.page) throw new Error("Failed to create page");
        await this.page.goto('https://www.doubao.com/chat/', { waitUntil: 'domcontentloaded' });
        await this.page.waitForTimeout(3000);
        
        const url = this.page.url();
        const title = await this.page.title();
        console.log(`[DoubaoClient-Debug] 当前页面 URL: ${url}`);
        console.log(`[DoubaoClient-Debug] 当前页面 Title: ${title}`);
        
        const userAgent = await this.page.evaluate(() => navigator.userAgent);
        console.log(`[DoubaoClient-Debug] 当前 User-Agent: ${userAgent}`);

        const loginTextVisible = await this.page.locator('text="登录/注册"').isVisible().catch(() => false);
        const hasLoginModal = url.includes('login') || loginTextVisible;

        if (hasLoginModal) {
            console.log('\n=============================================');
            console.log('需要登录豆包');
            
            if (headless) {
                console.error('当前处于无头模式(Headless)，无法进行手动登录。');
                console.error('请运行带 --ui 参数的命令进行首次登录');
                console.log('=============================================\n');
                throw new Error("Login required but running in headless mode.");
            }
            
            console.log('请在打开的浏览器窗口中完成登录。');
            console.log('=============================================\n');
            
            await this.page.screenshot({ path: 'debug-login-state.png' });
            console.log('[DoubaoClient-Debug] 已保存当前页面截图到 debug-login-state.png');
            
            console.log('[DoubaoClient] 等待用户登录...');
            await this.page.waitForSelector('textarea', { timeout: 0 }); 
            console.log('[DoubaoClient] 检测到输入框，登录成功！继续执行。');
        } else {
            console.log('[DoubaoClient] 已检测到登录状态。');
        }
    }

    /**
     * 设置网络响应拦截器，捕获 image_pre_watermark 原图响应
     */
    private startIntercepting() {
        if (!this.page) return;
        this.interceptedBuffers.clear();
        this.responseHandler = async (response: any) => {
            const url = response.url();
            // 拦截豆包图片 CDN 响应：包含 flow-imagex-sign 且不是缩略图/运营图
            if (url.includes('flow-imagex-sign') && !url.includes('downsize') && !url.includes('web-operation') && !url.includes('avatar')) {
                try {
                    const buffer = await response.body();
                    this.interceptedBuffers.set(url, buffer);
                    console.log(`[DoubaoClient] 网络拦截: 捕获图片响应 (${buffer.length} bytes)`);
                } catch (e) {
                    // ignore
                }
            }
        };
        this.page.on('response', this.responseHandler);
    }

    private stopIntercepting() {
        if (this.page && this.responseHandler) {
            this.page.off('response', this.responseHandler);
            this.responseHandler = null;
        }
    }

    async generateImage(options: { prompt: string, quality?: 'preview' | 'original', ratio?: string, timeout?: number }): Promise<string | null> {
        if (!this.page) throw new Error('Client not initialized. Call init() first.');

        const { prompt, quality = 'original', ratio, timeout = 120000 } = options;
        const finalPrompt = ratio ? `${prompt}，图片比例 ${ratio}` : prompt;
        
        console.log(`[DoubaoClient] 正在发送生图请求: ${finalPrompt} (要求质量: ${quality})`);

        try {
            // 启动网络拦截（专门拦截 image_pre_watermark 原图）
            this.startIntercepting();

            const inputLocator = this.page.locator('textarea').first();
            await inputLocator.waitFor({ state: 'visible', timeout: 10000 });
            await inputLocator.fill('');
            await inputLocator.fill(`帮我生成图片：${finalPrompt}`);
            await this.page.waitForTimeout(500);

            const beforeCount = await this.page.locator('img[src*="flow-imagex-sign"]').count();
            console.log(`[DoubaoClient-Debug] 发送指令前，检测到已有图片数量: ${beforeCount}`);

            await inputLocator.press('Enter');
            console.log('[DoubaoClient] 已发送指令，等待图片生成完成 (预计 10-30 秒)...');

            let targetUrl: string | null = null;
            let targetImgElement: any = null;
            const startTime = Date.now();
            let pollCount = 0;
            
            while (Date.now() - startTime < timeout) {
                await this.page.waitForTimeout(2000);
                pollCount++;
                const currentCount = await this.page.locator('img[src*="flow-imagex-sign"]').count();
                console.log(`[DoubaoClient-Debug] 第 ${pollCount} 次轮询检查, 当前图片数量: ${currentCount}`);
                
                if (currentCount > beforeCount) {
                    const imgLocators = await this.page.locator('img[src*="flow-imagex-sign"]').all();
                    targetImgElement = imgLocators[imgLocators.length - 1];
                    
                    // 等待图片加载完成
                    await this.page.waitForTimeout(3000);
                    targetUrl = await targetImgElement.getAttribute('src');
                    console.log(`[DoubaoClient] 检测到新图片生成: ${targetUrl?.substring(0, 80)}...`);
                    break;
                }
            }

            if (!targetUrl || !targetImgElement) {
                console.warn('[DoubaoClient] 等待图片超时');
                return null;
            }

            // 如果只需要预览图，直接返回
            if (quality === 'preview') {
                return targetUrl;
            }

            // === 获取原图：点击缩略图打开模态框 → 点击保存按钮 → 从 DOM/网络中提取原图 URL ===
            console.log('[DoubaoClient] 正在尝试获取原始大图...');
            
            // 1. 点击缩略图打开模态框（这会触发浏览器自动加载原图）
            await targetImgElement.evaluate((node: HTMLElement) => node.click());
            await this.page.waitForTimeout(3000);
            console.log('[DoubaoClient] 已打开大图模态框');

            // 2. 点击保存/下载按钮，触发原图确认
            let clickedSave = false;
            
            // 策略 A: 查找文字为"保存"或"下载"的可见按钮
            for (const text of ['保存', '下载', 'Save', 'Download']) {
                const els = await this.page.locator(`button:has-text("${text}"), [role="button"]:has-text("${text}")`).all();
                for (const el of els) {
                    try {
                        if (await el.isVisible({ timeout: 1000 })) {
                            await el.click();
                            clickedSave = true;
                            console.log(`[DoubaoClient] 已点击"${text}"按钮`);
                            break;
                        }
                    } catch (e) {
                        // ignore
                    }
                }
                if (clickedSave) break;
            }
            
            // 策略 B: 通过 aria-label 查找
            if (!clickedSave) {
                for (const label of ['保存', '下载', 'save', 'download']) {
                    const el = this.page.locator(`[aria-label="${label}" i]`).first();
                    try {
                        if (await el.count() > 0 && await el.isVisible({ timeout: 1000 })) {
                            await el.click();
                            clickedSave = true;
                            console.log(`[DoubaoClient] 已点击 aria-label="${label}" 按钮`);
                            break;
                        }
                    } catch (e) {
                        // ignore
                    }
                }
            }
            
            // 策略 C: 查找 SVG 下载图标
            if (!clickedSave) {
                const allSvgs = await this.page.locator('svg').all();
                for (const svg of allSvgs) {
                    try {
                        const html = await svg.evaluate(node => node.outerHTML);
                        if (html.includes('M19.207 12.707') || html.includes('M2 19C2') || html.includes('download') || html.includes('M4 16v')) {
                            const clickable = await svg.evaluateHandle(node => {
                                let el: HTMLElement | null = node as HTMLElement;
                                while (el && el.tagName !== 'BUTTON' && el.getAttribute('role') !== 'button') {
                                    el = el.parentElement;
                                    if (el && el.tagName === 'DIV') {
                                        const style = window.getComputedStyle(el);
                                        if (style.cursor === 'pointer') return el;
                                    }
                                }
                                return el;
                            });
                            const el = clickable.asElement();
                            if (el) {
                                await el.click();
                                clickedSave = true;
                                console.log('[DoubaoClient] 已点击 SVG 下载图标');
                                break;
                            }
                        }
                    } catch (e) {
                        // ignore
                    }
                }
            }

            if (!clickedSave) {
                console.log('[DoubaoClient] 未找到保存按钮，直接提取模态框中的图片 URL');
            }

            // 3. 等待原图加载完成
            await this.page.waitForTimeout(2000);

            // 4. 从模态框 DOM 中提取原图 URL
            const modalImages = await this.page.locator('img[src*="flow-imagex-sign"]').all();
            let bestUrl: string | null = null;
            
            for (const img of modalImages) {
                const src = await img.getAttribute('src');
                if (!src) continue;
                
                // 黄金标准：包含 image_pre_watermark 的是原图
                if (src.includes('image_pre_watermark')) {
                    console.log(`[DoubaoClient] 从 DOM 提取到原图 URL (image_pre_watermark)`);
                    bestUrl = src;
                    break;
                }
                
                // 排除缩略图/干扰项
                if (!src.includes('downsize') && !src.includes('web-operation') && !src.includes('avatar')) {
                    if (!bestUrl || src.length > bestUrl.length) {
                        bestUrl = src;
                    }
                }
            }

            // 5. 检查网络拦截是否捕获到了原图
            if (this.interceptedBuffers.size > 0) {
                const [firstUrl] = this.interceptedBuffers.keys();
                console.log(`[DoubaoClient] 网络拦截捕获到原图 (${this.interceptedBuffers.get(firstUrl)?.length} bytes)`);
                if (!bestUrl || bestUrl.includes('downsize')) {
                    bestUrl = firstUrl;
                }
            }

            // 6. 关闭模态框
            await this.page.keyboard.press('Escape');
            await this.page.waitForTimeout(500);

            if (bestUrl) {
                console.log(`[DoubaoClient] 最终原图 URL: ${bestUrl.substring(0, 80)}...`);
                return bestUrl;
            }

            // 回退到缩略图
            console.log('[DoubaoClient] 未能获取原图，回退到缩略图');
            return targetUrl;
            
        } catch (error) {
            console.error('[DoubaoClient] 生图过程发生错误:', error);
            return null;
        } finally {
            this.stopIntercepting();
        }
    }

    async close() {
        this.stopIntercepting();
        if (this.context) {
            await this.context.close();
            console.log('[DoubaoClient] 浏览器已关闭。');
        }
    }

    /**
     * 下载图片到本地
     * 优先使用浏览器网络拦截的原始数据（原图），否则用 Node.js https 下载
     */
    async downloadWithPage(url: string, destPath: string): Promise<string | null> {
        // 优先使用网络拦截的原始数据（原图）
        const intercepted = this.interceptedBuffers.get(url);
        if (intercepted) {
            console.log(`[DoubaoClient] 使用浏览器拦截的原图数据保存...`);
            const dir = path.dirname(destPath);
            if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
            fs.writeFileSync(destPath, intercepted);
            console.log(`[DoubaoClient] 图片已保存至: ${destPath} (${intercepted.length} bytes)`);
            return destPath;
        }

        // 使用静态方法下载
        return DoubaoClient.downloadImage(url, destPath);
    }

    static async downloadImage(url: string, destPath: string): Promise<string | null> {
        return new Promise((resolve) => {
            console.log(`[DoubaoClient] 正在下载图片至: ${destPath}`);
            
            const dir = path.dirname(destPath);
            if (!fs.existsSync(dir)) {
                fs.mkdirSync(dir, { recursive: true });
            }

            const file = fs.createWriteStream(destPath);
            
            https.get(url, {
                headers: {
                    'Referer': 'https://www.doubao.com/',
                    'User-Agent': 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36'
                }
            }, (response) => {
                if (response.statusCode !== 200) {
                    console.error(`[DoubaoClient] 下载失败，HTTP 状态码: ${response.statusCode}`);
                    file.close();
                    fs.unlink(destPath, () => {});
                    resolve(null);
                    return;
                }

                response.pipe(file);
                
                file.on('finish', () => {
                    file.close();
                    resolve(destPath);
                });
            }).on('error', (err) => {
                console.error(`[DoubaoClient] 下载发生错误: ${err.message}`);
                file.close();
                fs.unlink(destPath, () => {});
                resolve(null);
            });
        });
    }
}
