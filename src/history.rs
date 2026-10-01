//! Saves the conversation (the subtitles gathered so far) to a plain-text file, on request only.

use chrono::{DateTime, Local};
use std::io::Write;
use std::path::{Path, PathBuf};

const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

/// One subtitle as it should appear in the saved file.
pub struct Record<'a> {
    pub at: DateTime<Local>,
    pub lang: &'a str,
    pub original: &'a str,
    /// The Japanese text, when the line was translated.
    pub japanese: Option<&'a str>,
    /// A note for lines whose translation is missing, e.g. "翻訳中".
    pub note: Option<String>,
}

/// Where saved conversations go: the folder the user chose, else `LIVE_SUBTITLE_HISTORY_DIR`, else the Desktop.
pub fn history_dir(chosen: Option<&Path>) -> PathBuf {
    if let Some(dir) = chosen {
        return dir.to_path_buf();
    }
    if let Some(dir) = std::env::var_os("LIVE_SUBTITLE_HISTORY_DIR") {
        return dir.into();
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Desktop")
}

/// The folder as shown to the user: the home directory is written as `~`.
pub fn display_dir(dir: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    match dir.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => dir.display().to_string(),
    }
}

fn render(saved_at: DateTime<Local>, records: &[Record]) -> String {
    let mut text = format!("# Live Subtitle {}\n", saved_at.format(TIME_FORMAT));
    for r in records {
        text.push_str(&format!("\n[{}] {}\n{}\n", r.at.format(TIME_FORMAT), r.lang, r.original));
        if let Some(ja) = r.japanese {
            text.push_str(&format!("{ja}\n"));
        }
        if let Some(note) = &r.note {
            text.push_str(&format!("（{note}）\n"));
        }
    }
    text
}

/// Writes the records to a new file named after the current time inside `dir` and returns its path.
pub fn save(dir: &Path, records: &[Record]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let now = Local::now();
    let path = dir.join(format!("Live Subtitle {}.txt", now.format("%Y-%m-%d %H-%M-%S")));
    let mut file = std::fs::OpenOptions::new().create_new(true).write(true).open(&path)?;
    file.write_all(render(now, records).as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_original_translation_and_notes_in_order() {
        let at = Local::now();
        let records = [
            Record { at, lang: "en", original: "Hello there.", japanese: Some("やあ。"), note: None },
            Record { at, lang: "ja", original: "こんにちは。", japanese: None, note: None },
            Record { at, lang: "fr", original: "Bonjour.", japanese: None, note: Some("翻訳中".into()) },
        ];
        let text = render(at, &records);
        let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle}: {text}"));
        assert!(pos("Hello there.") < pos("やあ。"));
        assert!(pos("やあ。") < pos("こんにちは。"));
        assert!(pos("こんにちは。") < pos("Bonjour."));
        assert!(text.contains("（翻訳中）"));
    }

    #[test]
    fn save_creates_a_new_file_in_the_directory() {
        let dir = std::env::temp_dir().join(format!("live-subtitle-history-test-{}", std::process::id()));
        let records = [Record { at: Local::now(), lang: "en", original: "Hello there.", japanese: Some("やあ。"), note: None }];
        let path = save(&dir, &records).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("txt"));
        assert!(text.starts_with("# Live Subtitle ") && text.contains("Hello there.") && text.contains("やあ。"));
    }

    #[test]
    fn a_chosen_folder_wins_and_home_is_shown_as_tilde() {
        let chosen = Path::new("/somewhere/else");
        assert_eq!(history_dir(Some(chosen)), chosen);
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(display_dir(&home.join("Desktop")), "~/Desktop");
        assert_eq!(display_dir(chosen), "/somewhere/else");
    }
}
