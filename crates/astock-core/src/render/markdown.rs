//! Markdown 报告渲染
//!
//! 把 `SelectionReport` 渲染成 markdown 文本.
//! **这是解决 LLM hallucinate 的核心机制**: agent 直接发送这个字符串,
//! 不再自己"翻译" JSON, 从而避免用训练数据里的旧公司名替换真实名称.

use crate::model::report::SelectionReport;

/// 渲染选股报告为 markdown
pub fn render_selection_report(report: &SelectionReport) -> String {
    let mut out = String::with_capacity(2048);

    out.push_str("# 选股任务执行成功\n\n");

    // 结果摘要
    out.push_str("## 结果摘要\n\n");
    out.push_str(&format!("- 数据日期：{}\n", format_trade_date(&report.trade_date)));
    out.push_str(&format!("- 策略版本：{}\n", report.strategy));
    out.push_str(&format!("- 最终选出：{} 只\n\n", report.stocks.len()));

    // 筛选条件 (来自 filter_stats 反推)
    if let Some(s) = &report.filter_stats {
        out.push_str("## 筛选条件\n\n");
        // 根据 filter_stats 反推配置. 第 1 期采用硬编码默认值 (与 Python 版一致).
        out.push_str("- 涨幅：2.0-4.0%\n");
        out.push_str("- 换手率：5.0-10.0%\n");
        out.push_str("- 流通市值：50-200 亿\n");
        out.push_str("- 成交额：>3000 万\n");
        out.push_str("- 排除 ST / *ST\n");
        out.push_str("- 排除科创板 (688) / 北交所 (920)\n");
        out.push_str("- 排除新股 (上市 <60 天)\n\n");

        // 可选: 阶段统计
        out.push_str("## 筛选过程\n\n");
        out.push_str(&format!("- 初始：{} 只\n", s.initial));
        out.push_str(&format!("- 排除 ST 后：{} 只\n", s.after_exclude_st));
        out.push_str(&format!("- 排除科创板+北交所后：{} 只\n", s.after_exclude_star));
        out.push_str(&format!("- 排除新股后：{} 只\n", s.after_exclude_new));
        out.push_str(&format!("- 符合涨幅 2-4%：{} 只\n", s.after_change_pct));
        out.push_str(&format!("- 符合换手率 5-10%：{} 只\n", s.after_turnover));
        out.push_str(&format!("- 符合流通市值 50-200 亿：{} 只\n", s.after_circ_mv));
        out.push_str(&format!("- 符合成交额>3000 万：{} 只\n", s.after_amount));
        out.push_str(&format!("- 最终选出：{} 只\n\n", s.final_count));
    }

    // 选股结果表格
    if report.stocks.is_empty() {
        out.push_str("## 选股结果\n\n今日无符合全部条件的股票。\n\n");
    } else {
        out.push_str("## 选股结果\n\n");
        out.push_str("| 排名 | 代码 | 名称 | 涨幅% | 换手率% | 市值(亿) | 成交额(万) | 综合分数 |\n");
        out.push_str("|---:|:---|:---|---:|---:|---:|---:|---:|\n");
        for s in &report.stocks {
            out.push_str(&format!(
                "| {} | {} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.3} |\n",
                s.rank, s.ts_code, s.name,
                s.pct_chg, s.turnover_rate, s.circ_mv, s.amount, s.score,
            ));
        }
        out.push('\n');
    }

    out
}

/// YYYYMMDD -> YYYY-MM-DD
fn format_trade_date(s: &str) -> String {
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::report::{FilterStats, SelectedStock, SelectionReport};

    fn sample_report() -> SelectionReport {
        SelectionReport {
            trade_date: "20260630".into(),
            strategy: "v2.0 final".into(),
            stocks: vec![
                SelectedStock {
                    rank: 1,
                    ts_code: "300613.SZ".into(),
                    name: "富瀚微".into(),
                    industry: Some("半导体".into()),
                    close: 80.08,
                    pct_chg: 3.77,
                    turnover_rate: 8.51,
                    circ_mv: 175.86,
                    amount: 147002.20,
                    score: 0.917,
                    boost_hits: vec![],
                    boosted_score: None,
                },
                SelectedStock {
                    rank: 2,
                    ts_code: "600310.SH".into(),
                    name: "广西能源".into(),
                    industry: Some("水力发电".into()),
                    close: 4.22,
                    pct_chg: 3.94,
                    turnover_rate: 7.24,
                    circ_mv: 61.85,
                    amount: 44394.57,
                    score: 0.469,
                    boost_hits: vec![],
                    boosted_score: None,
                },
            ],
            filter_stats: Some(FilterStats {
                initial: 5532,
                after_exclude_st: 5315,
                after_exclude_star: 4399,
                after_exclude_new: 4391,
                after_change_pct: 579,
                after_turnover: 118,
                after_circ_mv: 49,
                after_amount: 49,
                final_count: 2,
            }),
            markdown: String::new(),
        }
    }

    #[test]
    fn test_render_contains_stock_names() {
        let report = sample_report();
        let md = render_selection_report(&report);
        // 关键: 名称必须原样出现, 不被 LLM 替换
        assert!(md.contains("富瀚微"), "markdown must contain original name 富瀚微");
        assert!(md.contains("广西能源"), "markdown must contain original name 广西能源");
        // 日期格式化
        assert!(md.contains("2026-06-30"));
    }

    #[test]
    fn test_render_table_format() {
        let report = sample_report();
        let md = render_selection_report(&report);
        // 表格头
        assert!(md.contains("| 排名 | 代码 | 名称 |"));
        // 数据行
        assert!(md.contains("| 1 | 300613.SZ | 富瀚微 |"));
        assert!(md.contains("| 2 | 600310.SH | 广西能源 |"));
    }

    #[test]
    fn test_render_empty() {
        let report = SelectionReport {
            trade_date: "20260701".into(),
            strategy: "v2.0 final".into(),
            stocks: vec![],
            filter_stats: None,
            markdown: String::new(),
        };
        let md = render_selection_report(&report);
        assert!(md.contains("今日无符合全部条件的股票"));
    }

    #[test]
    fn test_format_trade_date() {
        assert_eq!(format_trade_date("20260630"), "2026-06-30");
        assert_eq!(format_trade_date("not-a-date"), "not-a-date");
    }
}
