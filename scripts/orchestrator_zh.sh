#!/bin/bash
# orchestrator_zh.sh — 简化版流水线（中文）

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
source "$SCRIPT_DIR/lib.sh"

FEATURE=$1
if [ -z "$FEATURE" ]; then
  echo "需求不能为空！"
  echo "用法: ./tools/orchestrator_zh.sh <feature-name>"
  exit 1
fi

WORKTREES_DIR=".worktrees"

# Step 1: Architect 出设计
log_step "1/4" "架构师设计接口..."
run_claude 2 "$WORKTREES_DIR/architect" \
  "分析需求 $FEATURE，输出接口定义到 docs/interfaces/"

sync_worktree "$WORKTREES_DIR/architect" "design: $FEATURE" \
  "$WORKTREES_DIR/backend-dev" "$WORKTREES_DIR/ui-dev" \
  -- docs/interfaces docs/ui-specs

# Step 2: Developer 实现
log_step "2/4" "开发者实现..."
run_claude 2 "$WORKTREES_DIR/backend-dev" \
  "根据 docs/interfaces/ 实现 $FEATURE"

sync_worktree "$WORKTREES_DIR/backend-dev" "feat: $FEATURE" \
  "$WORKTREES_DIR/backend-tester" "$WORKTREES_DIR/reviewer" \
  -- src tests

# Step 3: 并行跑 Tester + Reviewer
log_step "3/4" "测试 + 审查（并行）..."
run_parallel 2 \
  "$WORKTREES_DIR/backend-tester" "为 $FEATURE 写完整测试" \
  "$WORKTREES_DIR/reviewer" "审查 $FEATURE 的代码变更"

sync_worktree "$WORKTREES_DIR/reviewer" "review: $FEATURE" \
  . "$WORKTREES_DIR/qa-lead" \
  -- docs/reviews

# Step 4: QA 汇总
log_step "4/4" "QA 质量门控..."
run_claude 2 "$WORKTREES_DIR/qa-lead" \
  "检查测试报告和审查报告，决定是否合并 $FEATURE"

log_ok "流水线完成: $FEATURE"
