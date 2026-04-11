//! Lightweight i18n for user-facing channel messages.
//!
//! CLI output and log messages stay in English.  Only strings sent back to
//! users through channels (WeChat, QQ, Telegram, etc.) are translated.
//!
//! Supported languages: en, zh, th, vi, ja, es, ko, ru, fr, de, json.

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

// ---------------------------------------------------------------------------
// Global default language (set once at gateway startup)
// ---------------------------------------------------------------------------

static DEFAULT_LANG: OnceLock<String> = OnceLock::new();

/// Set the default language for channel messages.  Call once at startup.
pub fn set_default_lang(lang: &str) {
    let _ = DEFAULT_LANG.set(resolve_lang(lang).to_owned());
}

/// Return the current default language code (falls back to "en").
pub fn default_lang() -> &'static str {
    DEFAULT_LANG.get().map(|s| s.as_str()).unwrap_or("en")
}

// ---------------------------------------------------------------------------
// Translation table
// ---------------------------------------------------------------------------

type LangMap = HashMap<&'static str, &'static str>;
type MsgMap = HashMap<&'static str, LangMap>;

static MESSAGES: LazyLock<MsgMap> = LazyLock::new(|| {
    let mut m: MsgMap = HashMap::new();

    macro_rules! msg {
        ($key:expr, $( $lang:expr => $val:expr ),+ $(,)?) => {{
            let mut lang_map: LangMap = HashMap::new();
            $( lang_map.insert($lang, $val); )+
            m.insert($key, lang_map);
        }};
    }

    msg!("pairing_required",
        "en" => "[\u{1F980}RsClaw] Pairing required. Your code: {code}\nApprove via: rsclaw channels pair {code}",
        "zh" => "[\u{1F980}RsClaw \u{63D0}\u{793A}] \u{9700}\u{8981}\u{914D}\u{5BF9}\u{9A8C}\u{8BC1}\u{3002}\u{4F60}\u{7684}\u{914D}\u{5BF9}\u{7801}: {code}\n\u{6279}\u{51C6}\u{547D}\u{4EE4}: rsclaw channels pair {code}",
        "th" => "[\u{1F980}RsClaw] \u{0E15}\u{0E49}\u{0E2D}\u{0E07}\u{0E01}\u{0E32}\u{0E23}\u{0E01}\u{0E32}\u{0E23}\u{0E08}\u{0E31}\u{0E1A}\u{0E04}\u{0E39}\u{0E48} \u{0E23}\u{0E2B}\u{0E31}\u{0E2A}\u{0E02}\u{0E2D}\u{0E07}\u{0E04}\u{0E38}\u{0E13}: {code}\n\u{0E2D}\u{0E19}\u{0E38}\u{0E21}\u{0E31}\u{0E15}\u{0E34}: rsclaw channels pair {code}",
        "vi" => "[\u{1F980}RsClaw] Can xac thuc ghep doi. Ma cua ban: {code}\nPhe duyet: rsclaw channels pair {code}",
        "ja" => "[\u{1F980}RsClaw] \u{30DA}\u{30A2}\u{30EA}\u{30F3}\u{30B0}\u{304C}\u{5FC5}\u{8981}\u{3067}\u{3059}\u{3002}\u{30B3}\u{30FC}\u{30C9}: {code}\n\u{627F}\u{8A8D}: rsclaw channels pair {code}",
        "es" => "[\u{1F980}RsClaw] Emparejamiento requerido. Tu codigo: {code}\nAprobar: rsclaw channels pair {code}",
        "ko" => "[\u{1F980}RsClaw] \u{D398}\u{C5B4}\u{B9C1}\u{C774} \u{D544}\u{C694}\u{D569}\u{B2C8}\u{B2E4}. \u{CF54}\u{B4DC}: {code}\n\u{C2B9}\u{C778}: rsclaw channels pair {code}",
        "ru" => "[\u{1F980}RsClaw] \u{0422}\u{0440}\u{0435}\u{0431}\u{0443}\u{0435}\u{0442}\u{0441}\u{044F} \u{0441}\u{043E}\u{043F}\u{0440}\u{044F}\u{0436}\u{0435}\u{043D}\u{0438}\u{0435}. \u{0412}\u{0430}\u{0448} \u{043A}\u{043E}\u{0434}: {code}\n\u{041E}\u{0434}\u{043E}\u{0431}\u{0440}\u{0438}\u{0442}\u{044C}: rsclaw channels pair {code}",
        "fr" => "[\u{1F980}RsClaw] Appairage requis. Votre code : {code}\nApprouver : rsclaw channels pair {code}",
        "de" => "[\u{1F980}RsClaw] Kopplung erforderlich. Ihr Code: {code}\nGenehmigen: rsclaw channels pair {code}",
    );

    msg!("pairing_queue_full",
        "en" => "Pairing queue full. Try again later.",
        "zh" => "配对队列已满，请稍后再试。",
        "fr" => "File d'appairage pleine. Réessayez plus tard.",
        "de" => "Kopplungswarteschlange voll. Versuchen Sie es später erneut.",
    );

    msg!("file_saved",
        "en" => "Saved {count} file(s) to uploads/",
        "zh" => "已保存 {count} 个文件到 uploads/",
        "th" => "บันทึก {count} ไฟล์ไปยัง uploads/ แล้ว",
        "vi" => "Da luu {count} tep vao uploads/",
        "ja" => "{count}件のファイルをuploads/に保存しました",
        "es" => "{count} archivo(s) guardado(s) en uploads/",
        "ko" => "{count}개 파일을 uploads/에 저장했습니다",
        "ru" => "Сохранено {count} файл(ов) в uploads/",
        "fr" => "{count} fichier(s) enregistré(s) dans uploads/",
        "de" => "{count} Datei(en) in uploads/ gespeichert",
    );

    msg!("file_menu",
        "en" => "Choose:\n1. Analyze & keep\n2. Analyze & delete\n3. Delete",
        "zh" => "请选择:\n1. 分析并保留\n2. 分析后删除\n3. 直接删除",
        "th" => "เลือก:\n1. วิเคราะห์และเก็บไว้\n2. วิเคราะห์แล้วลบ\n3. ลบ",
        "vi" => "Chon:\n1. Phan tich va giu\n2. Phan tich roi xoa\n3. Xoa",
        "ja" => "選択:\n1. 分析して保持\n2. 分析して削除\n3. 削除",
        "es" => "Elige:\n1. Analizar y conservar\n2. Analizar y eliminar\n3. Eliminar",
        "ko" => "선택:\n1. 분석 후 보관\n2. 분석 후 삭제\n3. 삭제",
        "ru" => "Выберите:\n1. Анализ и сохранить\n2. Анализ и удалить\n3. Удалить",
        "fr" => "Choisissez :\n1. Analyser et conserver\n2. Analyser et supprimer\n3. Supprimer",
        "de" => "Auswahl:\n1. Analysieren und behalten\n2. Analysieren und löschen\n3. Löschen",
    );

    msg!("file_size_exceeded",
        "en" => "File size exceeds limit ({limit} MB). Rejected:",
        "zh" => "文件超出大小限制 ({limit} MB)。已拒绝:",
        "th" => "ไฟล์เกินขนาดจำกัด ({limit} MB) ปฏิเสธ:",
        "vi" => "Tep vuot qua gioi han ({limit} MB). Tu choi:",
        "ja" => "ファイルサイズ制限超過 ({limit} MB)。拒否:",
        "es" => "Archivo excede el limite ({limit} MB). Rechazado:",
        "ko" => "파일 크기 제한 초과 ({limit} MB). 거부:",
        "ru" => "Файл превышает лимит ({limit} MB). Отклонено:",
        "fr" => "Le fichier dépasse la limite ({limit} Mo). Rejeté :",
        "de" => "Datei überschreitet das Limit ({limit} MB). Abgelehnt:",
    );

    msg!("file_size_adjust",
        "en" => "Adjust via /config_upload_size <MB>",
        "zh" => "可通过 /config_upload_size <MB> 调整",
        "th" => "ปรับได้ผ่าน /config_upload_size <MB>",
        "vi" => "Dieu chinh qua /config_upload_size <MB>",
        "ja" => "/config_upload_size <MB> で調整可能",
        "es" => "Ajustar con /config_upload_size <MB>",
        "ko" => "/config_upload_size <MB>로 조정 가능",
        "ru" => "Настроить через /config_upload_size <MB>",
        "fr" => "Ajuster via /config_upload_size <MB>",
        "de" => "Anpassen über /config_upload_size <MB>",
    );

    msg!("disk_space_low",
        "en" => "Insufficient disk space.\nRequired: {need} MB\nAvailable: {avail} MB",
        "zh" => "磁盘空间不足。\n需要: {need} MB\n可用: {avail} MB",
        "th" => "พื้นที่ดิสก์ไม่เพียงพอ\nต้องการ: {need} MB\nมี: {avail} MB",
        "vi" => "Khong du dung luong dia.\nCan: {need} MB\nCon: {avail} MB",
        "ja" => "ディスク容量不足。\n必要: {need} MB\n空き: {avail} MB",
        "es" => "Espacio en disco insuficiente.\nNecesario: {need} MB\nDisponible: {avail} MB",
        "ko" => "디스크 공간 부족.\n필요: {need} MB\n사용 가능: {avail} MB",
        "ru" => "Недостаточно места на диске.\nТребуется: {need} MB\nДоступно: {avail} MB",
        "fr" => "Espace disque insuffisant.\nRequis : {need} Mo\nDisponible : {avail} Mo",
        "de" => "Unzureichender Speicherplatz.\nBenötigt: {need} MB\nVerfügbar: {avail} MB",
    );

    msg!("no_extractable_content",
        "en" => "No extractable content.",
        "zh" => "无法提取内容。",
        "th" => "ไม่สามารถดึงเนื้อหาได้",
        "vi" => "Khong the trich xuat noi dung.",
        "ja" => "抽出可能なコンテンツがありません。",
        "es" => "Sin contenido extraible.",
        "ko" => "추출 가능한 콘텐츠가 없습니다.",
        "ru" => "Нет извлекаемого содержимого.",
        "fr" => "Aucun contenu extractible.",
        "de" => "Kein extrahierbarer Inhalt.",
    );

    msg!("no_extractable_deleted",
        "en" => "No extractable content. Files deleted.",
        "zh" => "无法提取内容。文件已删除。",
        "th" => "ไม่สามารถดึงเนื้อหาได้ ไฟล์ถูกลบแล้ว",
        "vi" => "Khong the trich xuat noi dung. Tep da bi xoa.",
        "ja" => "抽出可能なコンテンツがありません。ファイルを削除しました。",
        "es" => "Sin contenido extraible. Archivos eliminados.",
        "ko" => "추출 가능한 콘텐츠가 없습니다. 파일이 삭제되었습니다.",
        "ru" => "Нет извлекаемого содержимого. Файлы удалены.",
        "fr" => "Aucun contenu extractible. Fichiers supprimés.",
        "de" => "Kein extrahierbarer Inhalt. Dateien gelöscht.",
    );

    msg!("video_not_supported",
        "en" => "[Video message received. Video analysis is not yet supported.]",
        "zh" => "[收到视频消息。暂不支持视频分析。]",
        "th" => "[ได้รับวิดีโอ ยังไม่รองรับการวิเคราะห์วิดีโอ]",
        "vi" => "[Da nhan video. Chua ho tro phan tich video.]",
        "ja" => "[動画を受信。動画分析は未対応です。]",
        "es" => "[Video recibido. Analisis de video no soportado.]",
        "ko" => "[동영상 수신. 동영상 분석은 아직 지원되지 않습니다.]",
        "ru" => "[Получено видео. Анализ видео пока не поддерживается.]",
        "fr" => "[Vidéo reçue. L'analyse vidéo n'est pas encore prise en charge.]",
        "de" => "[Video empfangen. Videoanalyse wird noch nicht unterstützt.]",
    );

    msg!("describe_image",
        "en" => "Describe this image.",
        "zh" => "描述这张图片。",
        "th" => "อธิบายภาพนี้",
        "vi" => "Mo ta hinh anh nay.",
        "ja" => "この画像を説明してください。",
        "es" => "Describe esta imagen.",
        "ko" => "이 이미지를 설명해 주세요.",
        "ru" => "Опишите это изображение.",
        "fr" => "Décrivez cette image.",
        "de" => "Beschreiben Sie dieses Bild.",
    );
    msg!("describe_video",
        "en" => "Analyze this video.",
        "zh" => "分析一下这个视频。",
        "th" => "วิเคราะห์วิดีโอนี้",
        "vi" => "Phan tich video nay.",
        "ja" => "この動画を分析してください。",
        "es" => "Analiza este video.",
        "ko" => "이 동영상을 분석해 주세요.",
        "ru" => "Проанализируйте это видео.",
        "fr" => "Analysez cette vidéo.",
        "de" => "Analysieren Sie dieses Video.",
    );

    msg!("image_received",
        "en" => "[image received]",
        "zh" => "[已收到图片]",
        "th" => "[ได้รับภาพแล้ว]",
        "vi" => "[Da nhan hinh anh]",
        "ja" => "[画像を受信]",
        "es" => "[imagen recibida]",
        "ko" => "[이미지 수신]",
        "ru" => "[изображение получено]",
        "fr" => "[image reçue]",
        "de" => "[Bild empfangen]",
    );

    msg!("image_download_failed",
        "en" => "[image received but download failed]",
        "zh" => "[收到图片但下载失败]",
        "th" => "[ได้รับภาพแต่ดาวน์โหลดล้มเหลว]",
        "vi" => "[Da nhan hinh anh nhung tai xuong that bai]",
        "ja" => "[画像を受信しましたがダウンロードに失敗しました]",
        "es" => "[imagen recibida pero la descarga fallo]",
        "ko" => "[이미지를 수신했지만 다운로드에 실패했습니다]",
        "ru" => "[изображение получено, но загрузка не удалась]",
        "fr" => "[image reçue mais le téléchargement a échoué]",
        "de" => "[Bild empfangen, aber Download fehlgeschlagen]",
    );

    msg!("file_analyzable",
        "en" => "analyzable, ~{tokens} tokens",
        "zh" => "可分析, 约{tokens} tokens",
        "th" => "วิเคราะห์ได้, ~{tokens} tokens",
        "vi" => "co the phan tich, ~{tokens} tokens",
        "ja" => "分析可能, 約{tokens} tokens",
        "es" => "analizable, ~{tokens} tokens",
        "ko" => "분석 가능, ~{tokens} tokens",
        "ru" => "можно проанализировать, ~{tokens} tokens",
        "fr" => "analysable, ~{tokens} tokens",
        "de" => "analysierbar, ~{tokens} Tokens",
    );

    msg!("file_binary",
        "en" => "binary",
        "zh" => "二进制文件",
        "th" => "ไฟล์ไบนารี",
        "vi" => "tep nhi phan",
        "ja" => "バイナリ",
        "es" => "binario",
        "ko" => "바이너리",
        "ru" => "бинарный",
        "fr" => "binaire",
        "de" => "Binärdatei",
    );

    msg!("analyzing",
        "en" => "Analyzing file content...",
        "zh" => "正在分析文件内容...",
        "th" => "กำลังวิเคราะห์เนื้อหาไฟล์...",
        "vi" => "Dang phan tich noi dung tep...",
        "ja" => "ファイル内容を分析中...",
        "es" => "Analizando contenido del archivo...",
        "ko" => "파일 내용 분석 중...",
        "ru" => "Анализ содержимого файла...",
        "fr" => "Analyse du contenu du fichier...",
        "de" => "Dateiinhalt wird analysiert...",
    );

    msg!("analysis_timeout",
        "en" => "Analysis timed out.",
        "zh" => "分析超时。",
        "th" => "การวิเคราะห์หมดเวลา",
        "vi" => "Phan tich het thoi gian.",
        "ja" => "分析がタイムアウトしました。",
        "es" => "El analisis expiro.",
        "ko" => "분석 시간이 초과되었습니다.",
        "ru" => "Время анализа истекло.",
        "fr" => "L'analyse a expiré.",
        "de" => "Analyse-Zeitüberschreitung.",
    );

    msg!("analysis_failed",
        "en" => "Analysis failed.",
        "zh" => "分析失败。",
        "th" => "การวิเคราะห์ล้มเหลว",
        "vi" => "Phan tich that bai.",
        "ja" => "分析に失敗しました。",
        "es" => "El analisis fallo.",
        "ko" => "분석에 실패했습니다.",
        "ru" => "Анализ не удался.",
        "fr" => "L'analyse a échoué.",
        "de" => "Analyse fehlgeschlagen.",
    );

    msg!("files_deleted",
        "en" => "Files deleted.",
        "zh" => "文件已删除。",
        "th" => "ลบไฟล์แล้ว",
        "vi" => "Tep da bi xoa.",
        "ja" => "ファイルを削除しました。",
        "es" => "Archivos eliminados.",
        "ko" => "파일이 삭제되었습니다.",
        "ru" => "Файлы удалены.",
        "fr" => "Fichiers supprimés.",
        "de" => "Dateien gelöscht.",
    );

    msg!("image_attachment_received",
        "en" => "[image attachment received]",
        "zh" => "[收到图片附件]",
        "th" => "[ได้รับไฟล์แนบภาพ]",
        "vi" => "[Da nhan tep dinh kem hinh anh]",
        "ja" => "[画像添付ファイルを受信]",
        "es" => "[imagen adjunta recibida]",
        "ko" => "[이미지 첨부 수신]",
        "ru" => "[получено вложение с изображением]",
        "fr" => "[pièce jointe image reçue]",
        "de" => "[Bildanhang empfangen]",
    );

    msg!("image_file_received",
        "en" => "[image file received]",
        "zh" => "[收到图片文件]",
        "th" => "[ได้รับไฟล์ภาพ]",
        "vi" => "[Da nhan tep hinh anh]",
        "ja" => "[画像ファイルを受信]",
        "es" => "[archivo de imagen recibido]",
        "ko" => "[이미지 파일 수신]",
        "ru" => "[получен файл изображения]",
        "fr" => "[fichier image reçu]",
        "de" => "[Bilddatei empfangen]",
    );

    msg!("video_message_received",
        "en" => "[video message received]",
        "zh" => "[收到视频消息]",
        "th" => "[ได้รับวิดีโอ]",
        "vi" => "[Da nhan video]",
        "ja" => "[動画メッセージを受信]",
        "es" => "[mensaje de video recibido]",
        "ko" => "[동영상 메시지 수신]",
        "ru" => "[получено видео сообщение]",
        "fr" => "[message vidéo reçu]",
        "de" => "[Videonachricht empfangen]",
    );

    // -----------------------------------------------------------------------
    // CLI interactive prompts (en + zh only — developer-facing)
    // -----------------------------------------------------------------------

    // Setup
    msg!("cli_setup_title",
        "en" => "rsclaw setup",
        "zh" => "rsclaw 初始化",
        "fr" => "rsclaw configuration initiale",
        "de" => "rsclaw Einrichtung",
    );
    msg!("cli_migration_mode",
        "en" => "Migration mode",
        "zh" => "迁移模式",
        "fr" => "Mode de migration",
        "de" => "Migrationsmodus",
    );
    msg!("cli_import_desc",
        "en" => "Import  -- copy data into ~/.rsclaw (recommended)",
        "zh" => "导入  -- 将数据复制到 ~/.rsclaw（推荐）",
        "fr" => "Importer  -- copier les données dans ~/.rsclaw (recommandé)",
        "de" => "Importieren  -- Daten nach ~/.rsclaw kopieren (empfohlen)",
    );
    msg!("cli_fresh_desc",
        "en" => "New     -- ignore OpenClaw data, start new",
        "zh" => "全新  -- 忽略 OpenClaw 数据，从零开始",
        "fr" => "Nouveau  -- ignorer les données OpenClaw, recommencer",
        "de" => "Neu     -- OpenClaw-Daten ignorieren, neu starten",
    );
    msg!("cli_default_language",
        "en" => "Default language",
        "zh" => "默认语言",
        "fr" => "Langue par défaut",
        "de" => "Standardsprache",
    );
    msg!("cli_setup_complete",
        "en" => "Setup complete",
        "zh" => "初始化完成",
        "fr" => "Configuration terminée",
        "de" => "Einrichtung abgeschlossen",
    );
    msg!("cli_detected_openclaw",
        "en" => "Detected OpenClaw at {path}",
        "zh" => "检测到 OpenClaw 位于 {path}",
        "fr" => "OpenClaw détecté à {path}",
        "de" => "OpenClaw erkannt unter {path}",
    );

    // Configure
    msg!("cli_configure_section",
        "en" => "Configure section",
        "zh" => "配置项",
        "fr" => "Section de configuration",
        "de" => "Konfigurationsabschnitt",
    );
    msg!("cli_save_exit",
        "en" => "[Save & Exit]",
        "zh" => "[保存并退出]",
        "fr" => "[Enregistrer et quitter]",
        "de" => "[Speichern und beenden]",
    );
    msg!("cli_gateway",
        "en" => "Gateway (port, bind)",
        "zh" => "网关（端口、绑定）",
        "fr" => "Passerelle (port, liaison)",
        "de" => "Gateway (Port, Bindung)",
    );
    msg!("cli_model_provider",
        "en" => "Model Provider (provider, API key, model)",
        "zh" => "模型提供商（提供商、API 密钥、模型）",
        "fr" => "Fournisseur de modèle (fournisseur, clé API, modèle)",
        "de" => "Modellanbieter (Anbieter, API-Schlüssel, Modell)",
    );
    msg!("cli_channels",
        "en" => "Channels (add/remove)",
        "zh" => "消息通道（添加/删除）",
        "fr" => "Canaux (ajouter/supprimer)",
        "de" => "Kanäle (hinzufügen/entfernen)",
    );
    msg!("cli_web_search",
        "en" => "Web Search (provider, API keys)",
        "zh" => "网络搜索（提供商、API 密钥）",
        "fr" => "Recherche web (fournisseur, clés API)",
        "de" => "Websuche (Anbieter, API-Schlüssel)",
    );
    msg!("cli_upload_limits",
        "en" => "Upload Limits (file size, text chars)",
        "zh" => "上传限制（文件大小、文本字符数）",
        "fr" => "Limites de téléversement (taille fichier, caractères texte)",
        "de" => "Upload-Limits (Dateigröße, Textzeichen)",
    );
    msg!("cli_exec_safety",
        "en" => "Exec Safety (on/off)",
        "zh" => "执行安全（开/关）",
        "fr" => "Sécurité d'exécution (on/off)",
        "de" => "Ausführungssicherheit (ein/aus)",
    );
    msg!("cli_no_changes",
        "en" => "No changes made.",
        "zh" => "未做任何更改。",
        "fr" => "Aucune modification effectuée.",
        "de" => "Keine Änderungen vorgenommen.",
    );
    msg!("cli_cancelled",
        "en" => "Cancelled -- no changes saved.",
        "zh" => "已取消 -- 未保存任何更改。",
        "fr" => "Annulé -- aucune modification enregistrée.",
        "de" => "Abgebrochen -- keine Änderungen gespeichert.",
    );
    msg!("cli_saved_to",
        "en" => "Saved to {path}",
        "zh" => "已保存到 {path}",
        "fr" => "Enregistré dans {path}",
        "de" => "Gespeichert unter {path}",
    );

    // Onboard
    msg!("cli_choose_provider",
        "en" => "Choose a provider",
        "zh" => "选择提供商",
        "fr" => "Choisir un fournisseur",
        "de" => "Anbieter auswählen",
    );
    msg!("cli_enter_api_key",
        "en" => "Enter API key",
        "zh" => "输入 API 密钥",
        "fr" => "Entrez la clé API",
        "de" => "API-Schlüssel eingeben",
    );
    msg!("cli_choose_channels",
        "en" => "Choose channels to enable",
        "zh" => "选择要启用的消息通道",
        "fr" => "Choisir les canaux à activer",
        "de" => "Zu aktivierende Kanäle auswählen",
    );
    msg!("cli_onboard_complete",
        "en" => "Onboard complete",
        "zh" => "向导完成",
        "fr" => "Assistant terminé",
        "de" => "Einrichtungsassistent abgeschlossen",
    );

    // Setup wizard (onboard)
    msg!("cli_setup_wizard_title",
        "en" => "rsclaw -- setup wizard",
        "zh" => "rsclaw -- 设置向导",
        "fr" => "rsclaw -- assistant de configuration",
        "de" => "rsclaw -- Einrichtungsassistent",
    );
    msg!("cli_press_esc_back",
        "en" => "Press ESC to go back to the previous step",
        "zh" => "按 ESC 返回上一步",
        "fr" => "Appuyez sur ÉCHAP pour revenir à l'étape précédente",
        "de" => "ESC drücken, um zum vorherigen Schritt zurückzukehren",
    );
    msg!("cli_setup_cancelled",
        "en" => "Setup cancelled.",
        "zh" => "设置已取消。",
        "fr" => "Configuration annulée.",
        "de" => "Einrichtung abgebrochen.",
    );
    msg!("cli_confirm_setup",
        "en" => "Proceed with setup?",
        "zh" => "继续执行初始化？",
        "fr" => "Continuer la configuration ?",
        "de" => "Mit der Einrichtung fortfahren?",
    );
    msg!("cli_step_agent",
        "en" => "1. Agent",
        "zh" => "1. 智能体",
        "fr" => "1. Agent",
        "de" => "1. Agent",
    );
    msg!("cli_agent_name",
        "en" => "Agent name",
        "zh" => "智能体名称",
        "fr" => "Nom de l'agent",
        "de" => "Agent-Name",
    );
    msg!("cli_step_model_provider",
        "en" => "2. Model Provider",
        "zh" => "2. 模型提供商",
        "fr" => "2. Fournisseur de modèle",
        "de" => "2. Modellanbieter",
    );
    msg!("cli_step_gateway",
        "en" => "3. Gateway",
        "zh" => "3. 网关",
        "fr" => "3. Passerelle",
        "de" => "3. Gateway",
    );
    msg!("cli_step_channels",
        "en" => "4. {label}",
        "zh" => "4. {label}",
        "fr" => "4. {label}",
        "de" => "4. {label}",
    );
    msg!("cli_add_channel",
        "en" => "Add a channel",
        "zh" => "添加消息通道",
        "fr" => "Ajouter un canal",
        "de" => "Kanal hinzufügen",
    );
    msg!("cli_add_another_channel",
        "en" => "Add another channel",
        "zh" => "添加其他消息通道",
        "fr" => "Ajouter un autre canal",
        "de" => "Weiteren Kanal hinzufügen",
    );
    msg!("cli_all_channels_configured",
        "en" => "All channels configured.",
        "zh" => "所有消息通道已配置。",
        "fr" => "Tous les canaux sont configurés.",
        "de" => "Alle Kanäle konfiguriert.",
    );
    msg!("cli_skip_done",
        "en" => "[Skip / Done]",
        "zh" => "[跳过 / 完成]",
        "fr" => "[Passer / Terminé]",
        "de" => "[Überspringen / Fertig]",
    );
    msg!("cli_default_model",
        "en" => "Default model",
        "zh" => "默认模型",
        "fr" => "Modèle par défaut",
        "de" => "Standardmodell",
    );
    msg!("cli_port",
        "en" => "Port",
        "zh" => "端口",
        "fr" => "Port",
        "de" => "Port",
    );
    msg!("cli_bind_mode",
        "en" => "Bind mode",
        "zh" => "绑定模式",
        "fr" => "Mode de liaison",
        "de" => "Bindungsmodus",
    );
    msg!("cli_next_start",
        "en" => "Next: rsclaw gateway start",
        "zh" => "下一步: rsclaw gateway start",
        "fr" => "Suivant : rsclaw gateway start",
        "de" => "Nächster Schritt: rsclaw gateway start",
    );
    msg!("cli_summary_config",
        "en" => "Config: {path}",
        "zh" => "配置文件: {path}",
        "fr" => "Config : {path}",
        "de" => "Konfiguration: {path}",
    );
    msg!("cli_summary_provider",
        "en" => "Provider: {label} ({name})",
        "zh" => "提供商: {label} ({name})",
        "fr" => "Fournisseur : {label} ({name})",
        "de" => "Anbieter: {label} ({name})",
    );
    msg!("cli_summary_model",
        "en" => "Model: {model}",
        "zh" => "模型: {model}",
        "fr" => "Modèle : {model}",
        "de" => "Modell: {model}",
    );
    msg!("cli_summary_agent",
        "en" => "Agent: {name}",
        "zh" => "智能体: {name}",
        "fr" => "Agent : {name}",
        "de" => "Agent: {name}",
    );
    msg!("cli_summary_port",
        "en" => "Port: {port}",
        "zh" => "端口: {port}",
        "fr" => "Port : {port}",
        "de" => "Port: {port}",
    );
    msg!("cli_summary_channels",
        "en" => "Channels: {names}",
        "zh" => "消息通道: {names}",
        "fr" => "Canaux : {names}",
        "de" => "Kanäle: {names}",
    );

    // Configure
    msg!("cli_configure_title",
        "en" => "rsclaw configure",
        "zh" => "rsclaw 配置",
        "fr" => "rsclaw configuration",
        "de" => "rsclaw Konfiguration",
    );
    msg!("cli_editing",
        "en" => "Editing: {path}",
        "zh" => "编辑: {path}",
        "fr" => "Édition : {path}",
        "de" => "Bearbeiten: {path}",
    );
    msg!("cli_press_esc",
        "en" => "Press ESC to go back",
        "zh" => "按 ESC 返回",
        "fr" => "Appuyez sur ÉCHAP pour revenir",
        "de" => "ESC drücken zum Zurückkehren",
    );
    msg!("cli_unknown_section",
        "en" => "Unknown section: {name}",
        "zh" => "未知配置项: {name}",
        "fr" => "Section inconnue : {name}",
        "de" => "Unbekannter Abschnitt: {name}",
    );
    msg!("cli_no_config_found",
        "en" => "No config file found. Run `rsclaw onboard` first.",
        "zh" => "未找到配置文件。请先运行 `rsclaw onboard`。",
        "fr" => "Aucun fichier de configuration trouvé. Exécutez d'abord `rsclaw onboard`.",
        "de" => "Keine Konfigurationsdatei gefunden. Führen Sie zuerst `rsclaw onboard` aus.",
    );
    msg!("cli_config_parse_failed",
        "en" => "Config parse failed: {err}\n\nTry `rsclaw doctor --fix` to auto-repair.",
        "zh" => "配置解析失败: {err}\n\n请尝试 `rsclaw doctor --fix` 自动修复。",
        "fr" => "Échec de l'analyse de la configuration : {err}\n\nEssayez `rsclaw doctor --fix` pour réparer automatiquement.",
        "de" => "Konfigurationsanalyse fehlgeschlagen: {err}\n\nVersuchen Sie `rsclaw doctor --fix` zur automatischen Reparatur.",
    );
    msg!("cli_restarting_gateway",
        "en" => "Restarting gateway...",
        "zh" => "正在重启网关...",
        "fr" => "Redémarrage de la passerelle...",
        "de" => "Gateway wird neu gestartet...",
    );
    msg!("cli_gateway_restarted",
        "en" => "Gateway restarted",
        "zh" => "网关已重启",
        "fr" => "Passerelle redémarrée",
        "de" => "Gateway neu gestartet",
    );
    msg!("cli_restart_failed",
        "en" => "Failed to restart: {err}. Start manually: rsclaw gateway start",
        "zh" => "重启失败: {err}。请手动启动: rsclaw gateway start",
        "fr" => "Échec du redémarrage : {err}. Démarrez manuellement : rsclaw gateway start",
        "de" => "Neustart fehlgeschlagen: {err}. Manuell starten: rsclaw gateway start",
    );

    // Section headers
    msg!("cli_section_gateway",
        "en" => "Gateway",
        "zh" => "网关",
        "fr" => "Passerelle",
        "de" => "Gateway",
    );
    msg!("cli_section_model_provider",
        "en" => "Model Provider",
        "zh" => "模型提供商",
        "fr" => "Fournisseur de modèle",
        "de" => "Modellanbieter",
    );
    msg!("cli_section_channels",
        "en" => "Channels",
        "zh" => "消息通道",
        "fr" => "Canaux",
        "de" => "Kanäle",
    );
    msg!("cli_section_web_search",
        "en" => "Web Search",
        "zh" => "网络搜索",
        "fr" => "Recherche web",
        "de" => "Websuche",
    );
    msg!("cli_section_upload_limits",
        "en" => "Upload Limits",
        "zh" => "上传限制",
        "fr" => "Limites de téléversement",
        "de" => "Upload-Limits",
    );
    msg!("cli_section_exec_safety",
        "en" => "Exec Safety",
        "zh" => "执行安全",
        "fr" => "Sécurité d'exécution",
        "de" => "Ausführungssicherheit",
    );

    // Channel config
    msg!("cli_channels_hint",
        "en" => "Space = toggle on/off, Enter = edit config, ESC = done",
        "zh" => "空格 = 开关切换, 回车 = 编辑配置, ESC = 完成",
        "fr" => "Espace = activer/désactiver, Entrée = modifier la config, ÉCHAP = terminé",
        "de" => "Leertaste = ein/aus, Enter = Konfig bearbeiten, ESC = fertig",
    );
    msg!("cli_channels_hint_short",
        "en" => "Space = toggle on/off | Enter = edit | ESC = done",
        "zh" => "空格 = 开关 | 回车 = 编辑 | ESC = 完成",
        "fr" => "Espace = activer/désactiver | Entrée = modifier | ÉCHAP = terminé",
        "de" => "Leertaste = ein/aus | Enter = bearbeiten | ESC = fertig",
    );
    msg!("cli_finished",
        "en" => "[Finished]",
        "zh" => "[完成]",
        "fr" => "[Terminé]",
        "de" => "[Fertig]",
    );
    msg!("cli_configured",
        "en" => "configured",
        "zh" => "已配置",
        "fr" => "configuré",
        "de" => "konfiguriert",
    );
    msg!("cli_starting_login",
        "en" => "Starting login flow...",
        "zh" => "正在启动登录流程...",
        "fr" => "Démarrage du processus de connexion...",
        "de" => "Anmeldevorgang wird gestartet...",
    );
    msg!("cli_login_failed",
        "en" => "Login failed: {err}",
        "zh" => "登录失败: {err}",
        "fr" => "Échec de la connexion : {err}",
        "de" => "Anmeldung fehlgeschlagen: {err}",
    );
    msg!("cli_login_later",
        "en" => "Run `rsclaw channels login {channel}` later.",
        "zh" => "稍后运行 `rsclaw channels login {channel}`。",
        "fr" => "Exécutez `rsclaw channels login {channel}` plus tard.",
        "de" => "Führen Sie später `rsclaw channels login {channel}` aus.",
    );
    msg!("cli_fallback_manual",
        "en" => "Falling back to manual input.",
        "zh" => "回退到手动输入。",
        "fr" => "Retour à la saisie manuelle.",
        "de" => "Zurück zur manuellen Eingabe.",
    );
    msg!("cli_scan_oauth",
        "en" => "Scan / OAuth login",
        "zh" => "扫码 / OAuth 登录",
        "fr" => "Scan / Connexion OAuth",
        "de" => "Scan / OAuth-Anmeldung",
    );
    msg!("cli_manual_input",
        "en" => "Manual input (appId + appSecret)",
        "zh" => "手动输入 (appId + appSecret)",
        "fr" => "Saisie manuelle (appId + appSecret)",
        "de" => "Manuelle Eingabe (appId + appSecret)",
    );
    msg!("cli_auth_method",
        "en" => "{label} auth method",
        "zh" => "{label} 认证方式",
        "fr" => "Méthode d'authentification {label}",
        "de" => "{label} Authentifizierungsmethode",
    );
    msg!("cli_no_fields",
        "en" => "No configurable fields for {label}.",
        "zh" => "{label} 没有可配置的字段。",
        "fr" => "Aucun champ configurable pour {label}.",
        "de" => "Keine konfigurierbaren Felder für {label}.",
    );
    msg!("cli_config_enter_keep",
        "en" => "{label} config (Enter = keep, select Edit to change):",
        "zh" => "{label} 配置 (回车 = 保留, 选择编辑以更改):",
        "fr" => "Config {label} (Entrée = conserver, sélectionner Modifier pour changer) :",
        "de" => "{label}-Konfiguration (Enter = beibehalten, Bearbeiten wählen zum Ändern):",
    );
    msg!("cli_config_label",
        "en" => "{label} config:",
        "zh" => "{label} 配置:",
        "fr" => "Config {label} :",
        "de" => "{label}-Konfiguration:",
    );
    msg!("cli_scan_rescan",
        "en" => "Scan / OAuth login (re-scan)",
        "zh" => "扫码 / OAuth 登录 (重新扫描)",
        "fr" => "Scan / Connexion OAuth (re-scan)",
        "de" => "Scan / OAuth-Anmeldung (erneut scannen)",
    );
    msg!("cli_manual_edit",
        "en" => "Manual edit",
        "zh" => "手动编辑",
        "fr" => "Modification manuelle",
        "de" => "Manuell bearbeiten",
    );
    msg!("cli_dm_policy",
        "en" => "DM Policy (current: {policy})",
        "zh" => "私聊策略 (当前: {policy})",
        "fr" => "Politique DM (actuel : {policy})",
        "de" => "DM-Richtlinie (aktuell: {policy})",
    );
    msg!("cli_group_policy",
        "en" => "Group Policy (current: {policy})",
        "zh" => "群聊策略 (当前: {policy})",
        "fr" => "Politique de groupe (actuel : {policy})",
        "de" => "Gruppenrichtlinie (aktuell: {policy})",
    );

    // Provider / model
    msg!("cli_provider",
        "en" => "Provider",
        "zh" => "提供商",
        "fr" => "Fournisseur",
        "de" => "Anbieter",
    );
    msg!("cli_current_key",
        "en" => "Current key: {key}",
        "zh" => "当前密钥: {key}",
        "fr" => "Clé actuelle : {key}",
        "de" => "Aktueller Schlüssel: {key}",
    );
    msg!("cli_change_api_key",
        "en" => "Change API key?",
        "zh" => "更改 API 密钥？",
        "fr" => "Changer la clé API ?",
        "de" => "API-Schlüssel ändern?",
    );
    msg!("cli_testing_connectivity",
        "en" => "Testing provider connectivity...",
        "zh" => "正在测试提供商连通性...",
        "fr" => "Test de la connectivité du fournisseur...",
        "de" => "Anbieter-Konnektivität wird getestet...",
    );
    msg!("cli_connection_ok",
        "en" => "Connection OK",
        "zh" => "连接正常",
        "fr" => "Connexion OK",
        "de" => "Verbindung OK",
    );
    msg!("cli_connection_failed",
        "en" => "Connection failed: {err}",
        "zh" => "连接失败: {err}",
        "fr" => "Échec de la connexion : {err}",
        "de" => "Verbindung fehlgeschlagen: {err}",
    );
    msg!("cli_fix_later",
        "en" => "You can still save and fix later.",
        "zh" => "你仍可以保存，稍后再修复。",
        "fr" => "Vous pouvez toujours enregistrer et corriger plus tard.",
        "de" => "Sie können trotzdem speichern und später beheben.",
    );
    msg!("cli_not_set",
        "en" => "(not set)",
        "zh" => "(未设置)",
        "fr" => "(non défini)",
        "de" => "(nicht gesetzt)",
    );

    // Search providers
    msg!("cli_search_provider",
        "en" => "Search provider",
        "zh" => "搜索提供商",
        "fr" => "Fournisseur de recherche",
        "de" => "Suchanbieter",
    );
    msg!("cli_ddg_selected",
        "en" => "DuckDuckGo selected (no API key needed)",
        "zh" => "已选择 DuckDuckGo（无需 API 密钥）",
        "fr" => "DuckDuckGo sélectionné (pas de clé API nécessaire)",
        "de" => "DuckDuckGo ausgewählt (kein API-Schlüssel erforderlich)",
    );

    // Upload limits
    msg!("cli_max_file_size",
        "en" => "Max file size (MB)",
        "zh" => "最大文件大小 (MB)",
        "fr" => "Taille max. du fichier (Mo)",
        "de" => "Max. Dateigröße (MB)",
    );
    msg!("cli_max_text_chars",
        "en" => "Max text chars",
        "zh" => "最大文本字符数",
        "fr" => "Caractères texte max.",
        "de" => "Max. Textzeichen",
    );
    msg!("cli_vision_support",
        "en" => "Model supports images (vision)",
        "zh" => "模型支持图片（视觉）",
        "fr" => "Le modèle prend en charge les images (vision)",
        "de" => "Modell unterstützt Bilder (Vision)",
    );

    // Exec safety
    msg!("cli_exec_current",
        "en" => "Current: {status}",
        "zh" => "当前: {status}",
        "fr" => "Actuel : {status}",
        "de" => "Aktuell: {status}",
    );
    msg!("cli_exec_enabled",
        "en" => "enabled",
        "zh" => "已启用",
        "fr" => "activé",
        "de" => "aktiviert",
    );
    msg!("cli_exec_disabled",
        "en" => "disabled",
        "zh" => "已禁用",
        "fr" => "désactivé",
        "de" => "deaktiviert",
    );
    msg!("cli_enable_exec_safety",
        "en" => "Enable exec safety rules?",
        "zh" => "启用执行安全规则？",
        "fr" => "Activer les règles de sécurité d'exécution ?",
        "de" => "Ausführungssicherheitsregeln aktivieren?",
    );
    msg!("cli_exec_safety_on",
        "en" => "Exec safety enabled (deny/confirm rules active)",
        "zh" => "执行安全已启用（拒绝/确认规则生效）",
        "fr" => "Sécurité d'exécution activée (règles refus/confirmation actives)",
        "de" => "Ausführungssicherheit aktiviert (Ablehnungs-/Bestätigungsregeln aktiv)",
    );
    msg!("cli_exec_safety_off",
        "en" => "Exec safety disabled (all commands allowed)",
        "zh" => "执行安全已禁用（允许所有命令）",
        "fr" => "Sécurité d'exécution désactivée (toutes les commandes autorisées)",
        "de" => "Ausführungssicherheit deaktiviert (alle Befehle erlaubt)",
    );

    // Setup: input_step helpers
    msg!("cli_keep",
        "en" => "Keep: {value}",
        "zh" => "保留: {value}",
        "fr" => "Conserver : {value}",
        "de" => "Beibehalten: {value}",
    );
    msg!("cli_edit",
        "en" => "Edit",
        "zh" => "编辑",
        "fr" => "Modifier",
        "de" => "Bearbeiten",
    );
    msg!("cli_back",
        "en" => "Back",
        "zh" => "返回",
        "fr" => "Retour",
        "de" => "Zurück",
    );

    // Setup: workspace copy / migration
    msg!("cli_copy_workspace_failed",
        "en" => "Failed to copy workspace {path}: {err}",
        "zh" => "工作区复制失败 {path}: {err}",
        "fr" => "Échec de la copie de l'espace de travail {path} : {err}",
        "de" => "Workspace-Kopie fehlgeschlagen {path}: {err}",
    );
    msg!("cli_copied_workspace",
        "en" => "Copied {src} -> {dest} ({count} items)",
        "zh" => "已复制 {src} -> {dest} ({count} 项)",
        "fr" => "Copié {src} -> {dest} ({count} éléments)",
        "de" => "Kopiert {src} -> {dest} ({count} Elemente)",
    );
    msg!("cli_converted_config",
        "en" => "Converted openclaw.json -> rsclaw.json5 (workspace paths updated)",
        "zh" => "已转换 openclaw.json -> rsclaw.json5（工作区路径已更新）",
        "fr" => "Converti openclaw.json -> rsclaw.json5 (chemins d'espace de travail mis à jour)",
        "de" => "Konvertiert openclaw.json -> rsclaw.json5 (Workspace-Pfade aktualisiert)",
    );
    msg!("cli_importing_sessions",
        "en" => "Importing {count} session(s)...",
        "zh" => "正在导入 {count} 个会话...",
        "fr" => "Importation de {count} session(s)...",
        "de" => "{count} Sitzung(en) werden importiert...",
    );
    msg!("cli_imported_sessions",
        "en" => "Imported {sessions} session(s), {messages} message(s)",
        "zh" => "已导入 {sessions} 个会话，{messages} 条消息",
        "fr" => "{sessions} session(s) importée(s), {messages} message(s)",
        "de" => "{sessions} Sitzung(en) importiert, {messages} Nachricht(en)",
    );
    msg!("cli_import_errors",
        "en" => "{count} error(s) during import",
        "zh" => "导入过程中出现 {count} 个错误",
        "fr" => "{count} erreur(s) lors de l'importation",
        "de" => "{count} Fehler beim Import",
    );
    msg!("cli_import_failed",
        "en" => "Import failed: {err}",
        "zh" => "导入失败: {err}",
        "fr" => "Échec de l'importation : {err}",
        "de" => "Import fehlgeschlagen: {err}",
    );
    msg!("cli_store_open_failed",
        "en" => "Could not open store: {err}",
        "zh" => "无法打开存储: {err}",
        "fr" => "Impossible d'ouvrir le stockage : {err}",
        "de" => "Speicher konnte nicht geöffnet werden: {err}",
    );
    msg!("cli_gateway_language_set",
        "en" => "gateway.language = {lang}",
        "zh" => "gateway.language = {lang}",
        "fr" => "gateway.language = {lang}",
        "de" => "gateway.language = {lang}",
    );

    // Channel login
    msg!("cli_scanning_qr",
        "en" => "Scanning QR code for Weixin login...",
        "zh" => "正在扫描微信登录二维码...",
        "fr" => "Scan du code QR pour la connexion Weixin...",
        "de" => "QR-Code für Weixin-Anmeldung wird gescannt...",
    );
    msg!("cli_login_success_bot",
        "en" => "Login successful! bot_id={id}",
        "zh" => "登录成功！bot_id={id}",
        "fr" => "Connexion réussie ! bot_id={id}",
        "de" => "Anmeldung erfolgreich! bot_id={id}",
    );
    msg!("cli_login_success_brand",
        "en" => "Login successful! brand={brand}",
        "zh" => "登录成功！brand={brand}",
        "fr" => "Connexion réussie ! brand={brand}",
        "de" => "Anmeldung erfolgreich! brand={brand}",
    );

    // Setup: import / migration
    msg!("cli_import_data_to",
        "en" => "Import: data will be copied to {path}",
        "zh" => "导入: 数据将复制到 {path}",
        "fr" => "Importer : les données seront copiées dans {path}",
        "de" => "Import: Daten werden nach {path} kopiert",
    );
    msg!("cli_using_dir",
        "en" => "Using {path}",
        "zh" => "使用 {path}",
        "fr" => "Utilisation de {path}",
        "de" => "Verwende {path}",
    );
    msg!("cli_edit_config",
        "en" => "Edit {path}",
        "zh" => "编辑 {path}",
        "fr" => "Modifier {path}",
        "de" => "Bearbeiten {path}",
    );
    msg!("cli_then_start",
        "en" => "Then run: rsclaw gateway start",
        "zh" => "然后运行: rsclaw gateway start",
        "fr" => "Puis exécutez : rsclaw gateway start",
        "de" => "Dann ausführen: rsclaw gateway start",
    );
    msg!("cli_data_summary",
        "en" => "{agents} agent(s), {sessions} session(s), {jsonl} JSONL file(s)",
        "zh" => "{agents} 个智能体, {sessions} 个会话, {jsonl} 个 JSONL 文件",
        "fr" => "{agents} agent(s), {sessions} session(s), {jsonl} fichier(s) JSONL",
        "de" => "{agents} Agent(en), {sessions} Sitzung(en), {jsonl} JSONL-Datei(en)",
    );
    msg!("cli_workspace_seeded",
        "en" => "{count} workspace file(s) in {path}",
        "zh" => "{count} 个工作空间文件位于 {path}",
        "fr" => "{count} fichier(s) d'espace de travail dans {path}",
        "de" => "{count} Workspace-Datei(en) in {path}",
    );

    // --- Background context (/btw) ---
    msg!("btw_added",
        "en" => "Added background context #{id}",
        "zh" => "已添加背景上下文 #{id}",
        "th" => "เพิ่มบริบทพื้นหลัง #{id} แล้ว",
        "vi" => "Da them ngu canh nen #{id}",
        "ja" => "バックグラウンドコンテキスト #{id} を追加しました",
        "es" => "Contexto de fondo #{id} agregado",
        "ko" => "배경 컨텍스트 #{id} 추가됨",
        "ru" => "Добавлен фоновый контекст #{id}",
        "fr" => "Contexte d'arrière-plan #{id} ajouté",
        "de" => "Hintergrundkontext #{id} hinzugefügt",
    );
    msg!("btw_added_ttl",
        "en" => "Added background context #{id} (expires in {turns} turns)",
        "zh" => "已添加背景上下文 #{id}（{turns}轮后过期）",
        "th" => "เพิ่มบริบทพื้นหลัง #{id} (หมดอายุใน {turns} รอบ)",
        "vi" => "Da them ngu canh nen #{id} (het han sau {turns} luot)",
        "ja" => "バックグラウンドコンテキスト #{id} を追加しました（{turns}ターンで期限切れ）",
        "es" => "Contexto de fondo #{id} agregado (expira en {turns} turnos)",
        "ko" => "배경 컨텍스트 #{id} 추가됨 ({turns}턴 후 만료)",
        "ru" => "Добавлен фоновый контекст #{id} (истекает через {turns} ходов)",
        "fr" => "Contexte d'arrière-plan #{id} ajouté (expire dans {turns} tours)",
        "de" => "Hintergrundkontext #{id} hinzugefügt (läuft in {turns} Runden ab)",
    );
    msg!("btw_added_global",
        "en" => "Added global background context #{id}",
        "zh" => "已添加全局背景上下文 #{id}",
        "th" => "เพิ่มบริบทพื้นหลังทั่วไป #{id} แล้ว",
        "vi" => "Da them ngu canh nen toan cuc #{id}",
        "ja" => "グローバルバックグラウンドコンテキスト #{id} を追加しました",
        "es" => "Contexto de fondo global #{id} agregado",
        "ko" => "글로벌 배경 컨텍스트 #{id} 추가됨",
        "ru" => "Добавлен глобальный фоновый контекст #{id}",
        "fr" => "Contexte d'arrière-plan global #{id} ajouté",
        "de" => "Globaler Hintergrundkontext #{id} hinzugefügt",
    );
    msg!("btw_list_empty",
        "en" => "No active background context",
        "zh" => "没有活跃的背景上下文",
        "th" => "ไม่มีบริบทพื้นหลังที่ใช้งานอยู่",
        "vi" => "Khong co ngu canh nen dang hoat dong",
        "ja" => "アクティブなバックグラウンドコンテキストはありません",
        "es" => "Sin contexto de fondo activo",
        "ko" => "활성 배경 컨텍스트 없음",
        "ru" => "Нет активного фонового контекста",
        "fr" => "Aucun contexte d'arrière-plan actif",
        "de" => "Kein aktiver Hintergrundkontext",
    );
    msg!("btw_cleared",
        "en" => "Background context cleared",
        "zh" => "背景上下文已清除",
        "th" => "ล้างบริบทพื้นหลังแล้ว",
        "vi" => "Da xoa ngu canh nen",
        "ja" => "バックグラウンドコンテキストをクリアしました",
        "es" => "Contexto de fondo limpiado",
        "ko" => "배경 컨텍스트 지워짐",
        "ru" => "Фоновый контекст очищен",
        "fr" => "Contexte d'arrière-plan effacé",
        "de" => "Hintergrundkontext gelöscht",
    );
    msg!("btw_removed",
        "en" => "Removed background context #{id}",
        "zh" => "已删除背景上下文 #{id}",
        "th" => "ลบบริบทพื้นหลัง #{id} แล้ว",
        "vi" => "Da xoa ngu canh nen #{id}",
        "ja" => "バックグラウンドコンテキスト #{id} を削除しました",
        "es" => "Contexto de fondo #{id} eliminado",
        "ko" => "배경 컨텍스트 #{id} 삭제됨",
        "ru" => "Удален фоновый контекст #{id}",
        "fr" => "Contexte d'arrière-plan #{id} supprimé",
        "de" => "Hintergrundkontext #{id} entfernt",
    );
    msg!("btw_not_found",
        "en" => "Background context #{id} not found",
        "zh" => "未找到背景上下文 #{id}",
        "th" => "ไม่พบบริบทพื้นหลัง #{id}",
        "vi" => "Khong tim thay ngu canh nen #{id}",
        "ja" => "バックグラウンドコンテキスト #{id} が見つかりません",
        "es" => "Contexto de fondo #{id} no encontrado",
        "ko" => "배경 컨텍스트 #{id}를 찾을 수 없음",
        "ru" => "Фоновый контекст #{id} не найден",
        "fr" => "Contexte d'arrière-plan #{id} introuvable",
        "de" => "Hintergrundkontext #{id} nicht gefunden",
    );

    // --- Processing indicator (timeout-triggered) ---
    msg!("processing",
        "en" => "Processing, please wait...",
        "zh" => "正在处理中，请稍候...",
        "th" => "กำลังประมวลผล กรุณารอสักครู่...",
        "vi" => "Dang xu ly, vui long cho...",
        "ja" => "処理中です。しばらくお待ちください...",
        "es" => "Procesando, por favor espere...",
        "ko" => "처리 중입니다. 잠시만 기다려 주세요...",
        "ru" => "Обработка, пожалуйста подождите...",
        "fr" => "Traitement en cours, veuillez patienter...",
        "de" => "Verarbeitung läuft, bitte warten...",
    );

    m
});

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Translate a message key to the given language.  Falls back to English.
pub fn t(key: &str, lang: &str) -> String {
    if lang == "json" {
        return format!("{{\"key\":\"{key}\",\"status\":\"ok\"}}");
    }
    MESSAGES
        .get(key)
        .and_then(|lm| lm.get(lang).or_else(|| lm.get("en")))
        .map(|s| (*s).to_owned())
        .unwrap_or_else(|| key.to_owned())
}

/// Translate with format arguments.
///
/// Example: `t_fmt("file_saved", "zh", &[("count", "3")])`
pub fn t_fmt(key: &str, lang: &str, args: &[(&str, &str)]) -> String {
    if lang == "json" {
        let pairs: Vec<String> = args.iter().map(|(k, v)| format!("\"{}\":\"{}\"", k, v)).collect();
        let extra = if pairs.is_empty() { String::new() } else { format!(",{}", pairs.join(",")) };
        return format!("{{\"key\":\"{key}\"{extra},\"status\":\"ok\"}}");
    }
    let mut text = t(key, lang);
    for (k, v) in args {
        text = text.replace(&format!("{{{k}}}"), v);
    }
    text
}

/// Resolve a human-readable or config language value to a language code.
///
/// Examples: "Chinese" -> "zh", "Thai" -> "th", "ja" -> "ja"
pub fn resolve_lang(config_lang: &str) -> &'static str {
    let l = config_lang.to_lowercase();
    if l.starts_with("zh") || l.starts_with("cn") || l.contains("chinese") || l.contains("中文") {
        "zh"
    } else if l.starts_with("th") || l.contains("thai") || l.contains("ไทย") {
        "th"
    } else if l.starts_with("vi") || l.contains("vietnam") || l.contains("tiếng việt") {
        "vi"
    } else if l.starts_with("ja") || l.contains("japan") || l.contains("日本") {
        "ja"
    } else if l.starts_with("es") || l.contains("spanish") || l.contains("español") {
        "es"
    } else if l.starts_with("ko") || l.contains("korean") || l.contains("한국") {
        "ko"
    } else if l.starts_with("ru") || l.contains("russian") || l.contains("русск") {
        "ru"
    } else if l.starts_with("fr") || l.contains("french") || l.contains("français") {
        "fr"
    } else if l.starts_with("de") || l.contains("german") || l.contains("deutsch") {
        "de"
    } else if l == "json" || l.contains("raw") {
        "json"
    } else {
        "en"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_english_default() {
        assert_eq!(t("no_extractable_content", "en"), "No extractable content.");
    }

    #[test]
    fn translate_chinese() {
        let msg = t("no_extractable_content", "zh");
        assert!(msg.contains("无法提取"));
    }

    #[test]
    fn translate_with_args() {
        let msg = t_fmt("file_saved", "en", &[("count", "3")]);
        assert_eq!(msg, "Saved 3 file(s) to uploads/");
    }

    #[test]
    fn translate_json_mode() {
        let msg = t_fmt("file_saved", "json", &[("count", "3")]);
        assert!(msg.contains("\"key\":\"file_saved\""));
        assert!(msg.contains("\"count\":\"3\""));
    }

    #[test]
    fn resolve_lang_chinese() {
        assert_eq!(resolve_lang("Chinese"), "zh");
        assert_eq!(resolve_lang("zh-CN"), "zh");
    }

    #[test]
    fn resolve_lang_thai() {
        assert_eq!(resolve_lang("Thai"), "th");
        assert_eq!(resolve_lang("th"), "th");
    }

    #[test]
    fn resolve_lang_unknown() {
        assert_eq!(resolve_lang("English"), "en");
        assert_eq!(resolve_lang("unknown"), "en");
    }

    #[test]
    fn fallback_to_english() {
        // Unknown language falls back to English
        assert_eq!(t("no_extractable_content", "xx"), "No extractable content.");
    }

    #[test]
    fn default_lang_fallback() {
        // Before set_default_lang is called, default is "en"
        assert_eq!(default_lang(), "en");
    }
}
