/**
 * Modal for the `ask_user` tool. The agent uses `ask_user` mid-turn to
 * collect a structured choice from the human — single-select,
 * multi-select, or free-text "Other".
 *
 * Subscribes globally to `rsclawWs.onAskUser`, normalises the prompt
 * into local state, and replies by injecting a regular user message
 * via `useChatStore.getState().onUserInput`. The backend agent then
 * parses the reply in its next turn — there is no dedicated reply
 * endpoint, by design (`/api/v1/computer-use/...` does NOT apply
 * here). See backend dev's spec for details.
 *
 * Reply text contract (chosen as the least-ambiguous form the agent
 * can parse with zero structured state on its side):
 *
 *   - Single-select: `"<1-based-index>) <label>"`
 *   - Multi-select:  `"<i1>) <l1>, <i2>) <l2>, …"`
 *   - "Other"  →     the user's raw text
 *   - Cancel   →     the literal string `"cancel"`
 *
 * The numbered fallback the agent ALSO emits as a regular text-delta
 * is intentionally NOT suppressed in the transcript — if the user
 * cancels the modal, the question is still in chat history.
 */

import { useCallback, useEffect, useMemo, useState } from "react";

import { rsclawWs, type AskUserPayload, type AskUserPrompt } from "../lib/rsclaw-ws";
import { useChatStore } from "../store";
import Locale from "../locales";

import styles from "./ask-user-modal.module.scss";

/**
 * Discriminated by `kind`. The "options" variant holds a set of
 * chosen option indices (0-based into `prompt.options`); the "other"
 * variant holds the user's free-text answer.
 */
type Selection =
  | { kind: "options"; indices: Set<number> }
  | { kind: "other"; text: string };

/** Default selection derived from the recommended_index (clamped). */
function initialSelection(prompt: AskUserPrompt): Selection {
  const rec = prompt.recommended_index;
  if (
    typeof rec === "number" &&
    Number.isInteger(rec) &&
    rec >= 0 &&
    rec < prompt.options.length
  ) {
    return { kind: "options", indices: new Set([rec]) };
  }
  return { kind: "options", indices: new Set() };
}

/** Build the reply text per the contract above. */
function buildReply(prompt: AskUserPrompt, sel: Selection): string {
  if (sel.kind === "other") return sel.text.trim();
  const pairs = Array.from(sel.indices)
    .sort((a, b) => a - b)
    .map((i) => `${i + 1}) ${prompt.options[i]?.label ?? ""}`)
    .filter((s) => !s.endsWith(") "));
  if (pairs.length === 0) return "";
  return pairs.join(", ");
}

export function AskUserModal() {
  const [pending, setPending] = useState<AskUserPayload | null>(null);
  const [selection, setSelection] = useState<Selection>({
    kind: "options",
    indices: new Set(),
  });
  const [submitting, setSubmitting] = useState(false);

  // Subscribe to ask_user events globally. A new prompt arriving
  // mid-modal replaces the old one defensively (backend contract says
  // this shouldn't happen, but defensive UX in case of agent misbehaviour).
  useEffect(() => {
    rsclawWs.connect();
    const unsub = rsclawWs.onAskUser((ev) => {
      // Reject obviously malformed prompts before they reach state.
      if (!ev.prompt?.question || !Array.isArray(ev.prompt.options)) return;
      setPending(ev);
      setSelection(initialSelection(ev.prompt));
      setSubmitting(false);
    });
    return unsub;
  }, []);

  const close = useCallback(() => {
    setPending(null);
    setSubmitting(false);
  }, []);

  // Push a user message and dismiss. Bypasses doSubmit's command-match
  // and input-queue logic — the reply is a direct chat input.
  const sendReply = useCallback(
    (text: string) => {
      if (submitting) return;
      setSubmitting(true);
      // Fire-and-forget; chatStore handles its own loading + WS round-trip.
      void useChatStore
        .getState()
        .onUserInput(text, [])
        .finally(() => close());
    },
    [submitting, close],
  );

  const onSubmit = useCallback(() => {
    if (!pending) return;
    const text = buildReply(pending.prompt, selection);
    if (!text) return;
    sendReply(text);
  }, [pending, selection, sendReply]);

  const onCancel = useCallback(() => {
    if (!pending) return;
    sendReply("cancel");
  }, [pending, sendReply]);

  // Esc shortcut (matches the permission dialog's UX).
  useEffect(() => {
    if (!pending) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onCancel();
      } else if (
        e.key === "Enter" &&
        (e.metaKey || e.ctrlKey)
      ) {
        // ⌘/Ctrl+Enter sends. Plain Enter is intentionally left for
        // the "Other" textarea to insert a newline.
        e.preventDefault();
        onSubmit();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [pending, onCancel, onSubmit]);

  // Derived UI state — keep these computed at render so the radio /
  // checkbox state is always in sync with `selection`.
  const submitDisabled = useMemo(() => {
    if (!pending || submitting) return true;
    if (selection.kind === "other") return selection.text.trim() === "";
    if (selection.indices.size === 0) return true;
    return false;
  }, [pending, submitting, selection]);

  if (!pending) return null;
  const prompt = pending.prompt;
  const multi = prompt.multi_select === true;
  const rec = prompt.recommended_index;
  const recValid =
    typeof rec === "number" && rec >= 0 && rec < prompt.options.length;

  const toggleOption = (index: number) => {
    setSelection((cur) => {
      if (cur.kind === "other") {
        // Switching back from Other to an option.
        return { kind: "options", indices: new Set([index]) };
      }
      const next = new Set(cur.indices);
      if (multi) {
        if (next.has(index)) next.delete(index);
        else next.add(index);
      } else {
        next.clear();
        next.add(index);
      }
      return { kind: "options", indices: next };
    });
  };

  const selectOther = (text: string) => {
    setSelection({ kind: "other", text });
  };

  const isOther = selection.kind === "other";
  const otherText = isOther ? selection.text : "";

  return (
    <div
      className={styles.mask}
      role="dialog"
      aria-modal="true"
      aria-labelledby="ask-user-question"
    >
      <div className={styles.card}>
        <div className={styles.header}>
          {prompt.header && (
            <span className={styles.chip}>{prompt.header}</span>
          )}
          <h2 id="ask-user-question" className={styles.question}>
            {prompt.question}
          </h2>
          {multi && (
            <div className={styles.multiHint}>
              {Locale.AskUser?.MultiSelectHint ?? "Pick one or more"}
            </div>
          )}
        </div>

        <div className={styles.options}>
          {prompt.options.map((opt, i) => {
            const checked =
              !isOther && (selection as { indices: Set<number> }).indices.has(i);
            const isRec = recValid && i === rec;
            return (
              <label
                key={i}
                className={`${styles.option} ${checked ? styles.optionChecked : ""}`}
              >
                <input
                  type={multi ? "checkbox" : "radio"}
                  name="ask-user-option"
                  checked={checked}
                  onChange={() => toggleOption(i)}
                />
                <div className={styles.optionBody}>
                  <div className={styles.optionLabel}>
                    <span>{opt.label}</span>
                    {isRec && (
                      <span className={styles.recBadge}>
                        {Locale.AskUser?.Recommended ?? "Recommended"}
                      </span>
                    )}
                  </div>
                  {opt.description && (
                    <div className={styles.optionDesc}>{opt.description}</div>
                  )}
                </div>
              </label>
            );
          })}

          {/* "Other" — always present so the user isn't trapped by an
              incomplete options list. Selecting it focuses the input. */}
          <label
            className={`${styles.option} ${isOther ? styles.optionChecked : ""}`}
          >
            <input
              type="radio"
              name="ask-user-option"
              checked={isOther}
              onChange={() => selectOther(otherText)}
            />
            <div className={styles.optionBody}>
              <div className={styles.optionLabel}>
                <span>{Locale.AskUser?.Other ?? "Other"}</span>
              </div>
              <input
                type="text"
                className={styles.otherInput}
                placeholder={
                  Locale.AskUser?.OtherPlaceholder ?? "Type your answer…"
                }
                value={otherText}
                onChange={(e) => selectOther(e.target.value)}
                onFocus={() => {
                  if (!isOther) selectOther(otherText);
                }}
              />
            </div>
          </label>
        </div>

        <div className={styles.footer}>
          <span className={styles.escHint}>
            {Locale.AskUser?.EscHint ?? "Esc to cancel"}
          </span>
          <div className={styles.actions}>
            <button
              type="button"
              className={styles.btnGhost}
              onClick={onCancel}
              disabled={submitting}
            >
              {Locale.AskUser?.Cancel ?? "Cancel"}
            </button>
            <button
              type="button"
              className={styles.btnPrimary}
              onClick={onSubmit}
              disabled={submitDisabled}
              autoFocus={!multi && (selection as { indices?: Set<number> }).indices?.size === 1}
            >
              {Locale.AskUser?.Send ?? "Send"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}
