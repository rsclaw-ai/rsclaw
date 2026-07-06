//! Tushare 数据源
//!
//! tushare pro API 文档: <https://tushare.pro/document/2>
//!
//! 覆盖接口 (第 1 期):
//! - `trade_cal`: 交易日历 (用于找最近交易日)
//! - `stock_basic`: A 股列表
//! - `daily`: 日线行情
//! - `daily_basic`: 每日基本面 (换手率/流通市值等)
//!
//! # tushare 单位注意
//!
//! - `circ_mv`: 流通市值, **万元**
//! - `amount` (daily): 成交额, **千元**
//! - `amount` (daily_basic): 成交额, **万元** (与 daily 不同!)
//! - `vol`: 成交量, **手**
//! - `pct_chg`: 涨跌幅, **%** (直接是百分比数值, 例如 3.77)
//! - `turnover_rate`: 换手率, **%**

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::debug;

use crate::capability::{ConfigProvider, HttpCapability};

/// Tushare 数据源
pub struct TushareSource {
    http: std::sync::Arc<dyn HttpCapability>,
    token: String,
    endpoint: String,
}

impl TushareSource {
    /// 从配置里读 token (key: `tushare_token` 或 `tushare.token`)
    pub fn from_config(
        http: std::sync::Arc<dyn HttpCapability>,
        config: &dyn ConfigProvider,
    ) -> Result<Self> {
        let token = config
            .tushare_token()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("tushare token not configured. Set 'tushare_token' in config.")
            })?;
        Ok(Self {
            http,
            token,
            endpoint: "https://api.tushare.pro".to_string(),
        })
    }

    pub fn with_token(http: std::sync::Arc<dyn HttpCapability>, token: impl Into<String>) -> Self {
        Self {
            http,
            token: token.into(),
            endpoint: "https://api.tushare.pro".to_string(),
        }
    }

    /// 覆盖 API endpoint (测试用)
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// tushare 通用调用
    ///
    /// POST `{endpoint}` body: `{"api_name": "...", "token": "...", "params": {...}, "fields": "..."}`
    ///
    /// 返回 `data.items` (list of list) + `data.fields` (list of str).
    pub async fn call(
        &self,
        api_name: &str,
        params: Value,
        fields: &str,
    ) -> Result<TushareResponse> {
        let body = json!({
            "api_name": api_name,
            "token": self.token,
            "params": params,
            "fields": fields,
        });

        debug!(api = api_name, "tushare call");

        let resp: Value = self
            .http
            .post_json(&self.endpoint, &body)
            .await
            .with_context(|| format!("tushare {api_name} HTTP error"))?;

        // 检查业务层错误
        if let Some(code) = resp.get("code").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                bail!("tushare {api_name} error {code}: {msg}");
            }
        }

        let data = resp
            .get("data")
            .ok_or_else(|| anyhow::anyhow!("tushare {api_name}: missing data field"))?;

        let fields: Vec<String> = data
            .get("fields")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let items: Vec<Vec<Value>> = data
            .get("items")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Ok(TushareResponse { fields, items })
    }
}

/// tushare 接口响应
#[derive(Debug, Clone)]
pub struct TushareResponse {
    pub fields: Vec<String>,
    pub items: Vec<Vec<Value>>,
}

impl TushareResponse {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 按字段名取某行的某列 (字符串). 找不到返回 None.
    pub fn get_str(&self, row: usize, name: &str) -> Option<&str> {
        let col = self.fields.iter().position(|f| f == name)?;
        self.items.get(row)?.get(col)?.as_str()
    }

    /// 按字段名取 f64
    pub fn get_f64(&self, row: usize, name: &str) -> Option<f64> {
        let col = self.fields.iter().position(|f| f == name)?;
        let v = self.items.get(row)?.get(col)?;
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }

    /// 转成 Vec<ValueMap> 便于上层使用
    pub fn into_rows(self) -> Vec<ValueMap> {
        self.items
            .into_iter()
            .map(|row| {
                let mut m = ValueMap::default();
                for (i, v) in row.into_iter().enumerate() {
                    if let Some(k) = self.fields.get(i) {
                        m.0.insert(k.clone(), v);
                    }
                }
                m
            })
            .collect()
    }
}

/// 行数据 (字段名 -> 值)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValueMap(pub std::collections::HashMap<String, Value>);

impl ValueMap {
    pub fn str(&self, k: &str) -> Option<&str> {
        self.0.get(k).and_then(|v| v.as_str())
    }
    pub fn f64(&self, k: &str) -> Option<f64> {
        self.0
            .get(k)
            .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
    }
}

// ============================================================================
// 高层接口: 具体 tushare API
// ============================================================================

/// 股票基础信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TushareStock {
    pub ts_code: String,
    pub symbol: String,
    pub name: String,
    pub market: Option<String>,
    pub industry: Option<String>,
    pub list_date: Option<String>,
}

/// 日线行情
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TushareDaily {
    pub ts_code: String,
    pub trade_date: String,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// 成交量 (手)
    pub vol: f64,
    /// 成交额 (千元)
    pub amount: f64,
    /// 涨跌幅 (%)
    pub pct_chg: f64,
}

/// 每日基本面
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TushareDailyBasic {
    pub ts_code: String,
    pub trade_date: String,
    /// 换手率 (%)
    pub turnover_rate: f64,
    /// 流通市值 (万元)
    pub circ_mv: f64,
    /// 总市值 (万元)
    pub total_mv: Option<f64>,
    /// 市盈率
    pub pe: Option<f64>,
    /// 市净率
    pub pb: Option<f64>,
    /// 量比
    pub volume_ratio: Option<f64>,
}

impl TushareSource {
    /// 获取最近交易日 (从今天往前最多找 10 天)
    pub async fn latest_trade_date(&self) -> Result<String> {
        use chrono::{Duration, Utc};
        let today = Utc::now().date_naive();
        for offset in 0..10i64 {
            let day = today - Duration::days(offset);
            let ymd = day.format("%Y%m%d").to_string();
            let params = json!({
                "start_date": ymd,
                "end_date": ymd,
            });
            let resp = self.call("trade_cal", params, "cal_date,is_open").await?;
            for row in 0..resp.len() {
                if resp.get_str(row, "is_open") == Some("1") {
                    if let Some(d) = resp.get_str(row, "cal_date") {
                        return Ok(d.to_string());
                    }
                }
            }
        }
        bail!("cannot find recent trade date in last 10 days")
    }

    /// 获取全部上市股票
    pub async fn stock_basic(&self) -> Result<Vec<TushareStock>> {
        let params = json!({
            "exchange": "",
            "list_status": "L",
        });
        let resp = self
            .call("stock_basic", params, "ts_code,symbol,name,market,industry,list_date")
            .await?;
        let mut out = Vec::with_capacity(resp.len());
        for row in 0..resp.len() {
            out.push(TushareStock {
                ts_code: resp.get_str(row, "ts_code").unwrap_or("").to_string(),
                symbol: resp.get_str(row, "symbol").unwrap_or("").to_string(),
                name: resp.get_str(row, "name").unwrap_or("").to_string(),
                market: resp.get_str(row, "market").map(str::to_string),
                industry: resp.get_str(row, "industry").map(str::to_string),
                list_date: resp.get_str(row, "list_date").map(str::to_string),
            });
        }
        Ok(out)
    }

    /// 获取某日全部日线
    pub async fn daily(&self, trade_date: &str) -> Result<Vec<TushareDaily>> {
        let params = json!({ "trade_date": trade_date });
        let resp = self
            .call(
                "daily",
                params,
                "ts_code,trade_date,open,high,low,close,vol,amount,pct_chg",
            )
            .await?;
        let mut out = Vec::with_capacity(resp.len());
        for row in 0..resp.len() {
            let Some(ts_code) = resp.get_str(row, "ts_code") else {
                continue;
            };
            let Some(trade_date) = resp.get_str(row, "trade_date") else {
                continue;
            };
            out.push(TushareDaily {
                ts_code: ts_code.to_string(),
                trade_date: trade_date.to_string(),
                open: resp.get_f64(row, "open").unwrap_or(0.0),
                high: resp.get_f64(row, "high").unwrap_or(0.0),
                low: resp.get_f64(row, "low").unwrap_or(0.0),
                close: resp.get_f64(row, "close").unwrap_or(0.0),
                vol: resp.get_f64(row, "vol").unwrap_or(0.0),
                amount: resp.get_f64(row, "amount").unwrap_or(0.0),
                pct_chg: resp.get_f64(row, "pct_chg").unwrap_or(0.0),
            });
        }
        Ok(out)
    }

    /// 获取某日全部基本面
    pub async fn daily_basic(&self, trade_date: &str) -> Result<Vec<TushareDailyBasic>> {
        let params = json!({ "trade_date": trade_date });
        let resp = self
            .call(
                "daily_basic",
                params,
                "ts_code,trade_date,turnover_rate,circ_mv,total_mv,pe,pb,volume_ratio",
            )
            .await?;
        let mut out = Vec::with_capacity(resp.len());
        for row in 0..resp.len() {
            let Some(ts_code) = resp.get_str(row, "ts_code") else {
                continue;
            };
            let Some(trade_date) = resp.get_str(row, "trade_date") else {
                continue;
            };
            out.push(TushareDailyBasic {
                ts_code: ts_code.to_string(),
                trade_date: trade_date.to_string(),
                turnover_rate: resp.get_f64(row, "turnover_rate").unwrap_or(0.0),
                circ_mv: resp.get_f64(row, "circ_mv").unwrap_or(0.0),
                total_mv: resp.get_f64(row, "total_mv"),
                pe: resp.get_f64(row, "pe"),
                pb: resp.get_f64(row, "pb"),
                volume_ratio: resp.get_f64(row, "volume_ratio"),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tushare_response_parse() {
        let resp = TushareResponse {
            fields: vec!["ts_code".into(), "name".into(), "pct_chg".into()],
            items: vec![
                vec![
                    json!("000001.SZ"),
                    json!("平安银行"),
                    json!(2.5),
                ],
            ],
        };
        assert_eq!(resp.len(), 1);
        assert_eq!(resp.get_str(0, "ts_code"), Some("000001.SZ"));
        assert_eq!(resp.get_str(0, "name"), Some("平安银行"));
        assert_eq!(resp.get_f64(0, "pct_chg"), Some(2.5));
        assert_eq!(resp.get_f64(0, "missing"), None);
    }
}
