//! 综合评分
//!
//! v2.0 final 核心公式 (不含历史数据修正项):
//!
//! ```text
//! score_pct      = (pct_chg      - min) / (max - min)
//! score_turnover = (turnover_rate - min) / (max - min)
//! score_amount   = (amount        - min) / (max - min)
//!
//! score = score_pct * 0.4 + score_turnover * 0.3 + score_amount * 0.3
//! ```
//!
//! 归一化到 [0, 1] 区间. 当 max == min 时返回 0 (避免除零).

use super::filter::Candidate;

/// 给一批候选股票打分 (原地修改 `score` 字段).
///
/// 如果 `cands` 为空或所有候选在某指标上完全相等, 该指标得分为 0.
pub fn score_candidates(cands: &mut [Candidate]) {
    if cands.is_empty() {
        return;
    }

    let pct_min = cands.iter().map(|c| c.pct_chg).fold(f64::INFINITY, f64::min);
    let pct_max = cands.iter().map(|c| c.pct_chg).fold(f64::NEG_INFINITY, f64::max);
    let pct_range = pct_max - pct_min;

    let tr_min = cands.iter().map(|c| c.turnover_rate).fold(f64::INFINITY, f64::min);
    let tr_max = cands.iter().map(|c| c.turnover_rate).fold(f64::NEG_INFINITY, f64::max);
    let tr_range = tr_max - tr_min;

    let amt_min = cands.iter().map(|c| c.amount_wan).fold(f64::INFINITY, f64::min);
    let amt_max = cands.iter().map(|c| c.amount_wan).fold(f64::NEG_INFINITY, f64::max);
    let amt_range = amt_max - amt_min;

    for c in cands.iter_mut() {
        let s_pct = normalize(c.pct_chg, pct_min, pct_range);
        let s_tr = normalize(c.turnover_rate, tr_min, tr_range);
        let s_amt = normalize(c.amount_wan, amt_min, amt_range);
        c.score = s_pct * 0.4 + s_tr * 0.3 + s_amt * 0.3;
    }
}

/// 归一化单个值. `range == 0` 时返回 0.
fn normalize(v: f64, min: f64, range: f64) -> f64 {
    if range <= 0.0 {
        return 0.0;
    }
    (v - min) / range
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algo::filter::Candidate;

    fn cand(pct: f64, tr: f64, amt: f64) -> Candidate {
        Candidate {
            ts_code: format!("c{pct}"),
            name: "x".into(),
            industry: None,
            list_date: None,
            pct_chg: pct,
            turnover_rate: tr,
            circ_mv_yi: 100.0,
            amount_wan: amt,
            close: 10.0,
            score: 0.0,
        }
    }

    #[test]
    fn test_score_empty() {
        let mut v: Vec<Candidate> = vec![];
        score_candidates(&mut v);
        assert!(v.is_empty());
    }

    #[test]
    fn test_score_all_same_gives_zero() {
        let mut v = vec![cand(3.0, 7.0, 50000.0), cand(3.0, 7.0, 50000.0)];
        score_candidates(&mut v);
        for c in &v {
            assert!(c.score.abs() < 1e-9, "all-equal inputs should score 0");
        }
    }

    #[test]
    fn test_score_ordering() {
        // 更高涨幅 / 更高换手 / 更高成交额 -> 更高 score
        let mut v = vec![
            cand(2.0, 5.0, 10000.0), // 低
            cand(3.0, 7.5, 50000.0), // 中
            cand(4.0, 10.0, 100000.0), // 高
        ];
        score_candidates(&mut v);
        assert!(v[0].score < v[1].score);
        assert!(v[1].score < v[2].score);
        // 最高分应接近 1, 最低分应接近 0
        assert!((v[2].score - 1.0).abs() < 1e-9);
        assert!(v[0].score.abs() < 1e-9);
    }

    #[test]
    fn test_normalize_range_zero() {
        assert_eq!(normalize(3.0, 3.0, 0.0), 0.0);
    }
}
