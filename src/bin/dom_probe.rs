//! 一次性 DOM 诊断探针：打开最近一个历史对话， dump AI 回复气泡的 DOM 结构线索。
//! 只读操作，不发送任何消息、不触发生图。
//!
//! 用法: cargo run --release --bin dom_probe

use doubao_web_image::client::DoubaoClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut client = DoubaoClient::new()?;
    client.init(true).await?;

    // 打开侧栏最近一个历史对话
    let nav = client
        .debug_eval(
            r#"
            (function() {
                const links = Array.from(document.querySelectorAll('a[href*="/chat/"]'));
                for (const a of links) {
                    const href = a.getAttribute('href') || '';
                    if (/\/chat\/\d+/.test(href)) { location.href = href; return href; }
                }
                return '';
            })()
        "#,
        )
        .await;
    println!("navigated to: {nav:?}");
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;

    // 用与 collect_ai_reply_text 完全相同的 JS 验证选择器能抓到 AI 回复文字
    let dump = client
        .debug_eval(
            r#"
            (function() {
                const collect = (els, skipNestedSel) => {
                    const seen = new Set();
                    const out = [];
                    for (const el of els) {
                        if (skipNestedSel && el.parentElement && el.parentElement.closest(skipNestedSel)) continue;
                        const t = (el.innerText || '').trim();
                        if (!t || seen.has(t)) continue;
                        seen.add(t);
                        out.push(t);
                    }
                    return out;
                };
                const mdRoots = Array.from(document.querySelectorAll('div.md-box-root'))
                    .filter(el => !el.closest('[class*="thinking-box"], [class*="send-msg-bubble"]'));
                const parts = collect(mdRoots, 'div.md-box-root');
                const joined = parts.join('\n');
                return {
                    mdRootCount: mdRoots.length,
                    partCount: parts.length,
                    totalLen: joined.length,
                    head: joined.slice(0, 400),
                    tail: joined.slice(-200),
                    url: location.href
                };
            })()
        "#,
        )
        .await;
    match dump {
        Some(v) => println!("{}", serde_json::to_string_pretty(&v)?),
        None => println!("dump evaluate failed"),
    }

    client.close().await;
    Ok(())
}
