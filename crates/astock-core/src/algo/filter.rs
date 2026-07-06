//! 通用过滤
//!
//! 每个过滤函数接收 `Candidate` 列表, 返回过滤后的列表 + 剩余数量 (用于统计).
//! 过滤顺序与 Python 版 `selector_tushare_final.py` 保持一致.

use crate::model::stock::Stock;

/// 候选股票 (过滤/评分阶段的内部表示)
#[derive(Debug, Clone)]
pub struct Candidate {
    pub ts_code: String,
    pub name: String,
    pub industry: Option<String>,
    pub list_date: Option<String>,
    /// 涨跌幅 (%)
    pub pct_chg: f64,
    /// 换手率 (%)
    pub turnover_rate: f64,
    /// 流通市值 (亿元) —— 已从 tushare 的"万元"换算
    pub circ_mv_yi: f64,
    /// 成交额 (万元) —— 已从 tushare 的"千元"换算
    pub amount_wan: f64,
    /// 收盘价 (元)
    pub close: f64,
    /// 综合分数 (评分阶段填充)
    pub score: f64,
}

impl Candidate {
    /// 是否 ST (名称里含 ST)
    pub fn is_st(&self) -> bool {
        self.name.contains("ST")
    }

    /// 是否科创板/北交所 (688/920/8 字头)
    pub fn is_excluded_board(&self) -> bool {
        let code = &self.ts_code;
        code.starts_with("688") || code.starts_with("920") || code.starts_with('8')
    }

    /// 是否新股 (上市不足 N 天)
    pub fn is_new(&self, min_days: u32) -> bool {
        let Some(list_date) = &self.list_date else {
            return true; // 无上市日期, 保守视为新股
        };
        match (parse_ymd(list_date), ymd_today()) {
            (Some(ld), Some(today)) => {
                let days = today.signed_duration_since(ld).num_days();
                days < min_days as i64
            }
            _ => true,
        }
    }
}

/// YYYYMMDD 字符串 -> chrono NaiveDate
fn parse_ymd(s: &str) -> Option<chrono::NaiveDate> {
    if s.len() != 8 {
        return None;
    }
    let y: i32 = s[0..4].parse().ok()?;
    let m: u32 = s[4..6].parse().ok()?;
    let d: u32 = s[6..8].parse().ok()?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

fn ymd_today() -> Option<chrono::NaiveDate> {
    use chrono::Utc;
    Some(Utc::now().date_naive())
}

/// 排除 ST / *ST
pub fn exclude_st(cands: Vec<Candidate>) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands.into_iter().filter(|c| !c.is_st()).collect();
    let n = out.len();
    (out, n)
}

/// 排除科创板 (688) / 北交所 (920 / 8 字头)
pub fn exclude_star(cands: Vec<Candidate>) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands.into_iter().filter(|c| !c.is_excluded_board()).collect();
    let n = out.len();
    (out, n)
}

/// 排除新股 (上市不足 min_days 天)
pub fn exclude_new(cands: Vec<Candidate>, min_days: u32) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands
        .into_iter()
        .filter(|c| !c.is_new(min_days))
        .collect();
    let n = out.len();
    (out, n)
}

/// 过滤涨幅区间
pub fn filter_change_pct(cands: Vec<Candidate>, min: f64, max: f64) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands
        .into_iter()
        .filter(|c| c.pct_chg >= min && c.pct_chg <= max)
        .collect();
    let n = out.len();
    (out, n)
}

/// 过滤换手率区间
pub fn filter_turnover(cands: Vec<Candidate>, min: f64, max: f64) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands
        .into_iter()
        .filter(|c| c.turnover_rate >= min && c.turnover_rate <= max)
        .collect();
    let n = out.len();
    (out, n)
}

/// 过滤流通市值区间 (亿元)
pub fn filter_circ_mv(cands: Vec<Candidate>, min_yi: f64, max_yi: f64) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands
        .into_iter()
        .filter(|c| c.circ_mv_yi >= min_yi && c.circ_mv_yi <= max_yi)
        .collect();
    let n = out.len();
    (out, n)
}

/// 过滤成交额下限 (万元)
pub fn filter_amount(cands: Vec<Candidate>, min_wan: f64) -> (Vec<Candidate>, usize) {
    let out: Vec<_> = cands
        .into_iter()
        .filter(|c| c.amount_wan >= min_wan)
        .collect();
    let n = out.len();
    (out, n)
}

/// 合并 stock_basic 的 name/list_date/industry 到日线+基本面数据.
///
/// - 如果 daily_basic 里的 ts_code 在 stock_map 里找不到, 丢弃.
/// - 否则构造 Candidate (score 字段初始为 0, 留给评分阶段填充).
pub fn merge_into_candidates(
    dailies: &[crate::source::tushare::TushareDaily],
    basics: &[crate::source::tushare::TushareDailyBasic],
    stock_map: &std::collections::HashMap<String, Stock>,
) -> Vec<Candidate> {
    let daily_map: std::collections::HashMap<&str, &crate::source::tushare::TushareDaily> =
        dailies.iter().map(|d| (d.ts_code.as_str(), d)).collect();

    let mut out = Vec::with_capacity(basics.len());
    for b in basics {
        let Some(d) = daily_map.get(b.ts_code.as_str()) else {
            continue;
        };
        let Some(s) = stock_map.get(&b.ts_code) else {
            continue;
        };
        out.push(Candidate {
            ts_code: b.ts_code.clone(),
            name: s.name.clone(),
            industry: s.industry.clone(),
            list_date: s.list_date.clone(),
            pct_chg: d.pct_chg,
            turnover_rate: b.turnover_rate,
            // tushare circ_mv 单位: 万元 -> 亿: /10000
            circ_mv_yi: b.circ_mv / 10000.0,
            // tushare daily.amount 单位: 千元 -> 万: /10
            amount_wan: d.amount / 10.0,
            close: d.close,
            score: 0.0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(code: &str, name: &str) -> Candidate {
        Candidate {
            ts_code: code.into(),
            name: name.into(),
            industry: None,
            list_date: Some("20200101".into()),
            pct_chg: 3.0,
            turnover_rate: 7.0,
            circ_mv_yi: 100.0,
            amount_wan: 50000.0,
            close: 10.0,
            score: 0.0,
        }
    }

    #[test]
    fn test_exclude_st() {
        let v = vec![
            cand("000001.SZ", "平安银行"),
            cand("000002.SZ", "*ST 某某"),
            cand("000003.SZ", "ST 测试"),
            cand("000004.SZ", "正常股票"),
        ];
        let (out, n) = exclude_st(v);
        assert_eq!(n, 2);
        assert!(out.iter().all(|c| !c.is_st()));
    }

    #[test]
    fn test_exclude_star() {
        let v = vec![
            cand("000001.SZ", "主板"),
            cand("300001.SZ", "创业板"),
            cand("688001.SH", "科创板"),
            cand("920001.BJ", "北交所"),
            cand("830001.BJ", "北交所 8 字头"),
        ];
        let (out, n) = exclude_star(v);
        assert_eq!(n, 2);
        assert!(out.iter().all(|c| !c.is_excluded_board()));
    }

    #[test]
    fn test_filter_change_pct() {
        let v = vec![
            { let mut c = cand("1", "a"); c.pct_chg = 1.0; c },
            { let mut c = cand("2", "b"); c.pct_chg = 2.5; c },
            { let mut c = cand("3", "c"); c.pct_chg = 3.5; c },
            { let mut c = cand("4", "d"); c.pct_chg = 5.0; c },
        ];
        let (out, n) = filter_change_pct(v, 2.0, 4.0);
        assert_eq!(n, 2);
    }

    #[test]
    fn test_filter_circ_mv_yi() {
        let v = vec![
            { let mut c = cand("1", "a"); c.circ_mv_yi = 30.0; c },
            { let mut c = cand("2", "b"); c.circ_mv_yi = 100.0; c },
            { let mut c = cand("3", "c"); c.circ_mv_yi = 250.0; c },
        ];
        let (out, n) = filter_circ_mv(v, 50.0, 200.0);
        assert_eq!(n, 1);
        assert_eq!(out[0].ts_code, "2");
    }
}
