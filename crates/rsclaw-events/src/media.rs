//! Canonical media tokens and the wire shape of file attachments.
//!
//! Uploaded and generated media live under names of the form
//! `<src>_<kind>_<stem>.<ext>` (`up_i_202610271200abc.png`): `src` is the
//! provenance bucket (`up` upload, `dl` generated/downloaded, `a2a` from a
//! peer), `kind` one of `i`/`v`/`a`/`d`/`f`. Such a name, without the leading
//! `@`, is a *token*: the user can `@`-reference it and a remote client can
//! fetch it from `GET /api/v1/media/<token>`.
//!
//! Clients receive each attachment as `[name, mime, path, kind, token]`. The
//! first three are the original triple, so a client that reads only those
//! keeps working; `token` is `""` when the file has no canonical name.

use serde::{Serializer, ser::SerializeSeq};

/// Split a token into its bucket and kind letter, or `None` when `token` is
/// not a canonical media name. Accepts exactly what the `@`-reference
/// resolver accepts.
pub fn parse_token(token: &str) -> Option<(&str, char)> {
    let (src, rest) = token.split_once('_')?;
    if !matches!(src, "up" | "dl" | "a2a") {
        return None;
    }
    let kind = rest.chars().next().filter(|c| "ivdaf".contains(*c))?;
    let (stem, ext) = rest[1..].strip_prefix('_')?.split_once('.')?;
    let stem_ok = !stem.is_empty()
        && stem
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    let ext_ok = !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    (stem_ok && ext_ok).then_some((src, kind))
}

/// Directory under a bucket root holding files of `kind`.
pub fn category_for_kind(kind: char) -> &'static str {
    match kind {
        'i' => "images",
        'v' => "videos",
        'a' => "audios",
        'd' => "docs",
        _ => "files",
    }
}

/// The token for a file at `path`: its file name, when that name is
/// canonical and the file sits in the directory its kind belongs in.
pub fn token_for_path(path: &str) -> Option<String> {
    let mut parts = path.rsplit(['/', '\\']);
    let name = parts.next()?;
    let dir = parts.next()?;
    let (_, kind) = parse_token(name)?;
    (dir == category_for_kind(kind)).then(|| name.to_owned())
}

/// `image` / `video` / `audio` / `doc` / `file`, from the mime type, else
/// from the kind letter of a canonical name.
pub fn kind_label(mime: &str, name: &str) -> &'static str {
    let mime = mime.to_ascii_lowercase();
    if mime.starts_with("image/") {
        return "image";
    }
    if mime.starts_with("video/") {
        return "video";
    }
    if mime.starts_with("audio/") {
        return "audio";
    }
    if mime == "application/pdf"
        || mime.starts_with("text/")
        || mime.contains("officedocument")
        || mime.contains("msword")
    {
        return "doc";
    }
    let file_name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match parse_token(file_name).map(|(_, k)| k) {
        Some('i') => "image",
        Some('v') => "video",
        Some('a') => "audio",
        Some('d') => "doc",
        _ => "file",
    }
}

/// One attachment as clients see it: `[name, mime, path, kind, token]`.
pub fn file_entry(name: &str, mime: &str, path: &str) -> [String; 5] {
    [
        name.to_owned(),
        mime.to_owned(),
        path.to_owned(),
        kind_label(mime, name).to_owned(),
        token_for_path(path).unwrap_or_default(),
    ]
}

/// `files` as clients see them; see [`file_entry`].
pub fn files_json(files: &[(String, String, String)]) -> serde_json::Value {
    serde_json::Value::Array(
        files
            .iter()
            .map(|(n, m, p)| serde_json::json!(file_entry(n, m, p)))
            .collect(),
    )
}

/// Bring a stored `files` value up to the client shape: `[name, mime, path]`
/// rows (history written before kind and token existed) gain both.
pub fn normalize_files_json(files: &mut serde_json::Value) {
    let Some(rows) = files.as_array_mut() else {
        return;
    };
    for row in rows {
        let Some(cells) = row.as_array() else {
            continue;
        };
        let cell = |i: usize| cells.get(i).and_then(|v| v.as_str()).unwrap_or("");
        if cells.len() == 3 {
            *row = serde_json::json!(file_entry(cell(0), cell(1), cell(2)));
        }
    }
}

/// Header of the block an assistant message carries for the model, listing
/// the files its turn produced; history shown to people drops it
/// ([`strip_attachments_note`]).
pub const ATTACHMENTS_HEADER: &str = "\n\n[attachments]\n";

/// The block naming each produced file by `@token` (or name) and path, so a
/// later turn — whose tool results are gone — can still use the exact path.
/// `None` when there are no files.
pub fn attachments_note(files: &[(String, String, String)]) -> Option<String> {
    if files.is_empty() {
        return None;
    }
    let lines: Vec<String> = files
        .iter()
        .map(|(name, _, path)| match token_for_path(path) {
            Some(token) => format!("@{token} = {path}"),
            None => format!("{name} = {path}"),
        })
        .collect();
    Some(format!("{ATTACHMENTS_HEADER}{}", lines.join("\n")))
}

/// `text` without its trailing attachments block.
pub fn strip_attachments_note(text: &str) -> &str {
    text.split_once(ATTACHMENTS_HEADER).map_or(text, |(body, _)| body)
}

/// serde `serialize_with` for an event's `files`.
pub fn serialize_files<S: Serializer>(
    files: &[(String, String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut seq = serializer.serialize_seq(Some(files.len()))?;
    for (n, m, p) in files {
        seq.serialize_element(&file_entry(n, m, p))?;
    }
    seq.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_parse_like_the_resolver() {
        assert_eq!(parse_token("dl_i_202610271200abc.png"), Some(("dl", 'i')));
        assert_eq!(parse_token("up_d_202610271200abc_2.pdf"), Some(("up", 'd')));
        assert_eq!(parse_token("a2a_v_x1.mp4"), Some(("a2a", 'v')));
        assert_eq!(parse_token("rsclaw_tts_1.mp3"), None);
        assert_eq!(parse_token("dl_x_1.png"), None);
        assert_eq!(parse_token("dl_i_../x.png"), None);
        assert_eq!(parse_token("dl_i_1.png/x"), None);
        assert_eq!(parse_token("dl_i_.png"), None);
    }

    #[test]
    fn entries_carry_kind_and_token() {
        let e = file_entry(
            "dl_v_202610271200abc.mp4",
            "video/mp4",
            "/Users/a/Downloads/rsclaw/videos/dl_v_202610271200abc.mp4",
        );
        assert_eq!(e[3], "video");
        assert_eq!(e[4], "dl_v_202610271200abc.mp4");
        let tts = file_entry("rsclaw_tts_1.mp3", "audio/mpeg", "/tmp/rsclaw_tts_1.mp3");
        assert_eq!(tts[3], "audio");
        assert_eq!(tts[4], "");
        // A canonical name outside its kind's directory has no token.
        assert_eq!(token_for_path("/tmp/dl_i_1abc.png"), None);
        assert_eq!(
            token_for_path(r"C:\Users\u\Downloads\rsclaw\images\dl_i_1abc.png").as_deref(),
            Some("dl_i_1abc.png")
        );
    }

    #[test]
    fn attachments_note_names_tokens_and_paths() {
        let files = vec![
            ("dl_v_1abc.mp4".to_owned(), "video/mp4".to_owned(), r"C:\d\videos\dl_v_1abc.mp4".to_owned()),
            ("out.txt".to_owned(), "text/plain".to_owned(), "/w/out.txt".to_owned()),
        ];
        let note = attachments_note(&files).expect("note");
        assert_eq!(
            note,
            "\n\n[attachments]\n@dl_v_1abc.mp4 = C:\\d\\videos\\dl_v_1abc.mp4\nout.txt = /w/out.txt"
        );
        let text = format!("成片已发给你{note}");
        assert_eq!(strip_attachments_note(&text), "成片已发给你");
        assert_eq!(attachments_note(&[]), None);
    }

    #[test]
    fn old_rows_are_brought_up_to_shape() {
        let mut v = serde_json::json!([
            ["dl_i_1abc.png", "image/png", "/d/rsclaw/images/dl_i_1abc.png"],
            ["a", "b", "c", "file", ""]
        ]);
        normalize_files_json(&mut v);
        assert_eq!(v[0][3], "image");
        assert_eq!(v[0][4], "dl_i_1abc.png");
        assert_eq!(v[1].as_array().map(Vec::len), Some(5));
    }
}
