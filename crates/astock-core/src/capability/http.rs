//! HTTP 能力抽象 + 基于 reqwest 的默认实现

use std::collections::HashMap;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// HTTP 能力 trait
///
/// 抽象 HTTP 请求, 让 astock-core 能适配不同的 HTTP 后端:
/// - 原生环境: [`DefaultHttp`] (reqwest)
/// - WASM: 需实现基于 `web_sys::fetch` 的版本
/// - 测试: 可实现 mock 版本
#[async_trait]
pub trait HttpCapability: Send + Sync + 'static {
    /// GET 请求, 返回 body 文本
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String>;

    /// GET 请求, 返回 JSON
    async fn get_json(&self, url: &str) -> Result<Value> {
        let text = self.get(url, &[]).await?;
        let v: Value = serde_json::from_str(&text)?;
        Ok(v)
    }

    /// GET 请求, 返回原始 bytes
    ///
    /// 用于处理非 UTF-8 编码的响应 (如新浪 GBK)
    async fn get_bytes(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        // 默认实现: get() 后转 bytes (可能丢失 GBK 等编码)
        let text = self.get(url, headers).await?;
        Ok(text.into_bytes())
    }

    /// POST 请求, body 为 JSON, 返回 JSON
    async fn post_json(&self, url: &str, body: &Value) -> Result<Value>;

    /// POST 请求, body 为 form, 返回文本
    async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<String>;
}

/// 基于 reqwest 的默认 HTTP 实现
///
/// feature `http-reqwest` 启用时可用. 自带:
/// - 连接池
/// - 超时配置 (默认 30s)
/// - User-Agent 伪装
/// - 重定向跟随
#[cfg(feature = "http-reqwest")]
pub struct DefaultHttp {
    client: reqwest::Client,
}

#[cfg(feature = "http-reqwest")]
impl DefaultHttp {
    /// 创建新的 HTTP client
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
            .timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(8)
            .build()?;
        Ok(Self { client })
    }

    /// 使用自定义 reqwest client
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// HTTP 能力不可用占位
///
/// 用于测试或场景下明确标记 HTTP 不可用.
/// 所有方法都会返回错误.
pub struct NoHttp;

#[async_trait]
impl HttpCapability for NoHttp {
    async fn get(&self, url: &str, _headers: &[(&str, &str)]) -> Result<String> {
        anyhow::bail!("NoHttp: HTTP not available (GET {})", url);
    }

    async fn get_bytes(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        anyhow::bail!("NoHttp: HTTP not available (GET {})", url);
    }

    async fn post_json(&self, url: &str, _body: &Value) -> Result<Value> {
        anyhow::bail!("NoHttp: HTTP not available (POST {})", url);
    }

    async fn post_form(&self, url: &str, _form: &[(&str, &str)]) -> Result<String> {
        anyhow::bail!("NoHttp: HTTP not available (POST {})", url);
    }
}

#[cfg(feature = "http-reqwest")]
#[async_trait]
impl HttpCapability for DefaultHttp {
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String> {
        let mut req = self.client.get(url);
        for &(k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("HTTP GET {url} failed: {status}\n{body}");
        }
        Ok(body)
    }

    async fn get_bytes(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        let mut req = self.client.get(url);
        for &(k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if !status.is_success() {
            anyhow::bail!("HTTP GET {url} failed: {status}");
        }
        Ok(body.to_vec())
    }

    async fn post_json(&self, url: &str, body: &Value) -> Result<Value> {
        let resp = self.client.post(url).json(body).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("HTTP POST {url} failed: {status}\n{text}");
        }
        let v: Value = serde_json::from_str(&text)?;
        Ok(v)
    }

    async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<String> {
        let map: HashMap<&str, &str> = form.iter().copied().collect();
        let resp = self.client.post(url).form(&map).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("HTTP POST {url} failed: {status}\n{body}");
        }
        Ok(body)
    }
}
