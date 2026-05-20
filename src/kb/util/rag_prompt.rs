//! RAG citation-discipline prompt that the agent runtime should embed
//! in its system prompt **whenever the kb_* tools are exposed**.
//!
//! Spec §3 "RAG 引用纪律 Prompt" lists the five hard rules an agent
//! must follow when using kb_search results. Keeping the string here
//! (not in `src/agent/prompt_builder.rs`) means agent-side
//! integration is opt-in: callers `use rsclaw::kb::util::RAG_DISCIPLINE_PROMPT`
//! and append it to their system prefix only when kb tools are wired.
//!
//! The string is intentionally bilingual-friendly --short, declarative,
//! no model-specific phrasing.

/// Spec §3 §RAG 引用纪律. Drop this verbatim into the system prefix
/// when the kb_search / kb_fetch / kb_list_docs / kb_similar /
/// kb_search_entities tools are exposed to the agent.
pub const RAG_DISCIPLINE_PROMPT: &str = "\
[KB retrieval discipline --when kb_search results are in context]\n\
1. Returned chunks are *semantically* related, NOT exact matches. Before \
quoting any entity / brand / number, verify it actually appears in the \
chunk text --kb_search can return chunks about a different but similar \
entity.\n\
2. If `entity_alignment` shows `matched_chunks=0` for a keyword the user \
asked about, tell the user explicitly: \"the knowledge base has no data \
on <X>\" --do NOT substitute data from a different entity.\n\
3. Cite using `[^kb:<chunk_id>]` --the UI renders this as a clickable \
citation. Never invent a citation, never reuse a chunk_id you didn't \
receive in tool output.\n\
4. Documents you cannot see (visibility filter) won't appear in results. \
Do NOT speculate \"there should be a doc about X but I can't see it\" --\
report only what kb_search actually returned.\n\
5. Numbers and dates from the chunk are sourced from the user's KB; if \
the chunk's source is old, the data may be stale. When the user asks \
\"what's the current X?\", check the doc's `created_at` / `version` in \
`kb_fetch` output before answering.\n\
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_mentions_required_terms() {
        let p = RAG_DISCIPLINE_PROMPT;
        // Hard rules spec §3 enumerated: semantic-relevance, entity
        // alignment, citation format, visibility filter, recency.
        assert!(p.contains("entity_alignment"));
        assert!(p.contains("kb:<chunk_id>"));
        assert!(p.contains("visibility"));
        assert!(p.contains("semantically"));
    }

    #[test]
    fn prompt_is_ascii_safe_for_streaming() {
        // The prompt must not contain combining characters or
        // hard-to-tokenize multi-byte sequences that could destabilise
        // the KV cache (spec §3 KV cache 友好). ASCII-only.
        for c in RAG_DISCIPLINE_PROMPT.chars() {
            assert!(
                c.is_ascii(),
                "non-ASCII char {c:?} in RAG_DISCIPLINE_PROMPT"
            );
        }
    }
}
