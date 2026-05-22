/**
 * Thin wrapper around CodeMirror 6 for the JSON5 config view.
 *
 * Why CodeMirror 6 (not Monaco):
 *   - ~100KB gzipped vs Monaco's ~3MB. The config tab is incidental
 *     surface — we want syntax highlight + find/replace, not a full IDE.
 *   - `@codemirror/search` panel is built in (Cmd+F / Cmd+G next /
 *     Cmd+Shift+G prev / Cmd+Opt+F replace). No custom wiring needed.
 *
 * JSON5 specifics: there's no official `@codemirror/lang-json5`, so we
 * use the plain JSON grammar. JSON5-only constructs (single-quoted
 * strings, trailing commas, comments, unquoted keys) won't get
 * highlighted *errors* because we don't load a linter — they just
 * tokenise as best-effort. Validation still happens in the caller
 * via `JSON5.parse()` and shows as a separate banner.
 */

import CodeMirror from "@uiw/react-codemirror";
import { json } from "@codemirror/lang-json";
import { oneDark } from "@codemirror/theme-one-dark";

interface Json5EditorProps {
  value: string;
  onChange: (value: string) => void;
  /** Height passed straight through to CodeMirror. */
  height?: string;
}

export function Json5Editor({ value, onChange, height = "100%" }: Json5EditorProps) {
  return (
    <CodeMirror
      value={value}
      onChange={onChange}
      height={height}
      theme={oneDark}
      extensions={[json()]}
      basicSetup={{
        lineNumbers: true,
        foldGutter: true,
        highlightActiveLine: true,
        bracketMatching: true,
        autocompletion: false,
        // The search panel (Cmd+F) is enabled by default via the
        // `searchKeymap`. Leaving basicSetup's `searchKeymap: true`
        // implicit.
      }}
      style={{ height, fontSize: 12 }}
    />
  );
}
