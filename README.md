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
- 📎 **参考图上传**：`--reference` 上传本地参考图（最多 4 张），生图时保持主体/商品外观一致。
- 📚 **同对话批量生图**：`--batch=plan.json` 在一个对话里先发全文文案、再连续发多条配图要求，人物/风格一致性更好。
- 📏 **比例控制**：支持通过自然语言参数控制图片长宽比（如 `16:9`, `1:1`）。
- 🛡️ **验证码自动降级**：默认无头模式运行，遇到风控拦截时自动弹窗切换到 UI 模式。
- ⚡ **单文件分发**：编译后单个 exe，无需 Node.js、无需 npm install、无需单独下载浏览器。

## 📦 安装

### 方式一：下载预编译二进制（推荐）

从 [Releases](https://github.com/gushuaialan1/doubao-web-image/releases/latest) 页面下载对应平台的二进制文件：

| 平台 | 下载 | 大小 |
|------|------|------|
| Windows x64 | `doubao-web-image-windows-x64.zip` | ~3.7 MB |
| Linux x64 | `doubao-web-image-linux-x64.tar.gz` | ~3.5 MB |
| macOS (Apple Silicon / Intel*) | `doubao-web-image-macos.tar.gz` | ~3.2 MB |

\* Intel Mac 用户可通过 Rosetta 运行 ARM64 版本

**Windows 用户**：
- 确保系统已安装 Chrome 或 Edge（Chromium 内核）
- 解压后双击 `doubao-web-image.exe` 即可运行

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
| `--no-watermark` | 去除左上角「AI 生成」水印 | `--no-watermark` |
| `--reference` | 参考图路径（可重复，最多 4 张，支持逗号分隔） | `--reference=./cover.png` |
| `--batch` | 同对话批量生图模式（plan.json 路径） | `--batch=plan.json` |
| `--timeout-ms` | 每张图片的等待超时（毫秒；单图默认 120000，批量默认 180000） | `--timeout-ms=180000` |

支持的图片比例：`1:1`, `2:3`, `3:4`, `4:3`, `9:16`, `16:9`

### 同对话批量生图（--batch，保持人物/风格一致）

单图模式每次调用都是全新对话，人物一致性差。`--batch` 复刻网页版手动流程：**打开一个对话，先发送整篇文案建立上下文，再连续发送多条配图要求**，所有图片在同一个对话中生成，一致性显著更好。

```bash
doubao-web-image.exe --batch=plan.json --timeout-ms=180000
```

`plan.json` 格式：

```json
{
  "context": "可选。整篇文案，作为对话首条消息发出（纯文字，不期待图片产出），用于给豆包建立全文上下文",
  "ratio": "9:16",
  "quality": "original",
  "noWatermark": true,
  "items": [
    { "prompt": "场景1的完整生图 prompt", "output": "E:/output/scene_00.png" },
    { "prompt": "场景2的完整生图 prompt", "output": "E:/output/scene_01.png" }
  ]
}
```

行为说明：

- `context` / `ratio` / `quality` / `noWatermark` 均可选（`quality` 默认 `original`，`noWatermark` 默认 `false`）；`items` 必填且非空，否则报错退出。
- context 消息只等待 AI 回复停止（最长 30s），**不等待图片**。
- 逐条发送 item 的 prompt，等待并下载图片到各自 `output` 路径（ratio/quality/noWatermark 逻辑与单图模式一致）。
- **单张失败（超时/风控）只记录错误并继续下一张**，不整体中断。
- 全部完成后 stdout 输出一行 JSON 摘要，供调用方解析：

```json
{"results":[{"output":"E:/output/scene_00.png","ok":true},{"output":"E:/output/scene_01.png","ok":false,"error":"等待图片超时（180000ms）"}]}
```

- 进程退出码：**全部失败才非零**（至少一张成功即退出码 0）。plan.json 解析失败或 items 为空时非零。
- `--ui`（无头初始化失败时也会自动降级到 UI 模式）、`--timeout-ms`（每张图超时，默认 180s）同样可用。

### 参考图（保持主体外观一致）

通过 `--reference` 先把本地图片上传到聊天附件区，再发送提示词，豆包会参考这些图生成结果（等价于网页版里「先拖入参考图再发文案」）。支持 `png/jpg/jpeg/webp`，单次最多 4 张：

```bash
# 单张参考图
doubao-web-image.exe "参考这张图的构图，画一个古代书房场景" --reference=./room.png

# 多张参考图（逗号分隔或重复传参均可）
doubao-web-image.exe "参考这本书的封面和书脊，生成书桌上的商品展示图" --reference=./cover.png,./spine.png
```

推书场景示例（商品转化图，保持书籍外观一致）：

```bash
doubao-web-image.exe "参考这本书的装帧，生成一张放在复古木桌上的推书海报图，暖光氛围" --reference=./book-cover.png --ratio=3:4 --output=./book-promo.png
```

### 综合示例

### 去除水印

豆包生成的图片左上角带有「AI 生成」标签，可添加 `--no-watermark` 参数自动去除：

```bash
doubao-web-image.exe "一只金毛犬坐在草地上" --no-watermark --output=dog.png
```

**去水印原理**：优先拦截 `/chat/completion` 的 SSE 数据流，直接提取服务端返回的 `image_ori_raw` 无水印原图 URL（借鉴 [doubao-nomark](https://github.com/gushuaialan1/doubao-nomark) 的解析思路）。如果拦截失败，则回退到等比例放大+顶部裁切的老方案，画面无拉伸变形，仅损失顶部和左右少量边缘内容。

### 综合示例

```bash
doubao-web-image.exe "星空下的赛博朋克城市" --ratio=9:16 --quality=original --no-watermark --output=./city_wallpaper.png
```

## 🐛 常见问题

- **Q: 提示"未能获取到图片，可能触发了人机验证"？**
  - A: 脚本已内置自动重试机制。当在无头模式下遇到风控，脚本会自动关闭并以 UI 模式重启，给你在浏览器中手动完成验证的机会。

- **Q: 提示"找不到 Chrome 浏览器"？**
  - A: 本工具依赖系统中已有的 Chrome/Edge。请确保已安装 Chrome 或 Microsoft Edge。

- **Q: 生成的图片大小只有几百 KB？**
  - A: 确保没有加上 `--quality=preview` 参数。脚本默认会优先获取服务端返回的 `image_ori_raw` 无水印原图（通常 >1MB）；如果走回退逻辑，则获取 `image_pre_watermark` 级别的高清图。

## 🔧 技术栈

- **Rust** 2024 edition
- **chromiumoxide** 0.9 — CDP 浏览器自动化
- **tokio** — 异步运行时
- **clap** — CLI 参数解析
- **reqwest** — HTTP 下载 fallback

## 📄 License

MIT
