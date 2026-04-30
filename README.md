# Doubao Web Image Generation CLI (Rust)

基于 `chromiumoxide` 的豆包 (Doubao) Web 端网页自动化生图工具，单文件可执行，零运行时依赖。

> **注意**：这是 Rust 重构版本，位于 `rust-rewrite` 分支。TypeScript/Playwright 版本请查看 `main` 分支。

## ⚠️ 免责声明

**本项目仅供编程学习、浏览器自动化测试研究和技术交流使用。**
- 本项目并非豆包官方产品，与字节跳动公司无任何关联。
- 使用本项目产生的任何后果由使用者本人承担。
- **请勿将本项目用于任何非法、侵权、恶意刷量或商业牟利的场景。**

## 🌟 特性

- 🤖 **免 API Key**：通过 `chromiumoxide` 模拟浏览器操作，直接复用网页版登录状态。
- 🖼️ **高清大图下载**：自动拦截原生下载链接，获取 >3MB 的无损高分辨率原图。
- 📏 **比例控制**：支持通过自然语言参数控制图片长宽比（如 `16:9`, `1:1`）。
- 🛡️ **验证码自动降级**：默认无头模式运行，遇到风控拦截时自动弹窗切换到 UI 模式。
- ⚡ **单文件分发**：编译后单个 exe，无需 Node.js、无需 npm install、无需单独下载浏览器。

## 📦 安装

### 方式一：下载预编译二进制（推荐）

从 [Releases](https://github.com/gushuaialan1/doubao-web-image/releases) 页面下载对应平台的二进制文件。

**Windows 用户**：
- 确保系统已安装 Chrome 或 Edge（Chromium 内核）
- 双击 `doubao-web-image.exe` 即可运行

### 方式二：从源码编译

需要 Rust 1.85+。

```bash
git clone https://github.com/gushuaialan1/doubao-web-image.git
cd doubao-web-image
git checkout rust-rewrite
cargo build --release
```

编译产物位于 `target/release/doubao-web-image`（或 Windows 下的 `.exe`）。

## 🚀 使用方法

### 首次使用（需手动登录）

第一次运行**必须带上 `--ui` 参数**以打开可视化浏览器：

```bash
# Windows
doubao-web-image.exe "画一只可爱的猫咪" --ui

# Linux/macOS
./doubao-web-image "画一只可爱的猫咪" --ui
```

在弹出的浏览器中完成手机号/验证码登录后，程序会自动继续生成图片。登录态保存在本地的 `~/.doubao-web-session` 目录中，后续无需重复登录。

### 日常生图（后台无头模式）

登录成功后，可以直接在后台静默生成并下载图片：

```bash
doubao-web-image.exe "一只带有未来科技感的机器狗"
```

### 高级参数

| 参数 | 说明 | 示例 |
|------|------|------|
| `--ui` | 显示浏览器窗口（首次登录必须） | `--ui` |
| `--quality` | `preview` 或 `original`（默认） | `--quality=original` |
| `--ratio` | 图片比例 | `--ratio=9:16` |
| `--output` | 输出路径（默认 `generated.png`） | `--output=./wallpaper.png` |

支持的图片比例：`1:1`, `2:3`, `3:4`, `4:3`, `9:16`, `16:9`

### 综合示例

```bash
doubao-web-image.exe "星空下的赛博朋克城市" --ratio=9:16 --quality=original --output=./city_wallpaper.png
```

## 🐛 常见问题

- **Q: 提示"未能获取到图片，可能触发了人机验证"？**
  - A: 脚本已内置自动重试机制。当在无头模式下遇到风控，脚本会自动关闭并以 UI 模式重启，给你在浏览器中手动完成验证的机会。

- **Q: 提示"找不到 Chrome 浏览器"？**
  - A: 本工具依赖系统中已有的 Chrome/Edge。请确保已安装 Chrome 或 Microsoft Edge。

- **Q: 生成的图片大小只有几百 KB？**
  - A: 确保没有加上 `--quality=preview` 参数。脚本默认会获取 `image_pre_watermark` 级别的高清无损原图（通常 >1MB）。

## 🔧 技术栈

- **Rust** 2024 edition
- **chromiumoxide** 0.9 — CDP 浏览器自动化
- **tokio** — 异步运行时
- **clap** — CLI 参数解析
- **reqwest** — HTTP 下载 fallback

## 📄 License

MIT
