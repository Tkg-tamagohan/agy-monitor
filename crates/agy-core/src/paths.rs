use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::{CoreError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Surface {
    App,
    Cli,
    Ide,
    Backup,
}

impl Surface {
    pub const ALL: [Surface; 4] = [Surface::App, Surface::Cli, Surface::Ide, Surface::Backup];

    pub fn dir_name(self) -> &'static str {
        match self {
            Surface::App => "antigravity",
            Surface::Cli => "antigravity-cli",
            Surface::Ide => "antigravity-ide",
            Surface::Backup => "antigravity-backup",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Surface::App => "app",
            Surface::Cli => "cli",
            Surface::Ide => "ide",
            Surface::Backup => "backup",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppDataRoot {
    pub surface: Surface,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct GeminiPaths {
    pub gemini_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ProjectsJson {
    #[serde(default)]
    projects: HashMap<String, String>,
}

impl GeminiPaths {
    pub fn discover() -> Result<Self> {
        let home = dirs::home_dir().ok_or(CoreError::NoHomeDir)?;
        Ok(Self {
            gemini_dir: home.join(".gemini"),
        })
    }

    pub fn from_gemini_dir(gemini_dir: impl Into<PathBuf>) -> Self {
        Self {
            gemini_dir: gemini_dir.into(),
        }
    }

    pub fn app_data_roots(&self) -> Vec<AppDataRoot> {
        Surface::ALL
            .iter()
            .filter_map(|&surface| {
                let dir = self.gemini_dir.join(surface.dir_name());
                if dir.is_dir() {
                    Some(AppDataRoot { surface, dir })
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn projects_json_path(&self) -> PathBuf {
        self.gemini_dir.join("projects.json")
    }

    /// Reads `~/.gemini/projects.json` and returns `normalized_path_key -> project_name`.
    pub fn load_projects_json(&self) -> HashMap<String, String> {
        let path = self.projects_json_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return HashMap::new();
        };
        let Ok(parsed) = serde_json::from_str::<ProjectsJson>(&raw) else {
            return HashMap::new();
        };
        let mut out = HashMap::new();
        for (raw_path, name) in parsed.projects {
            let trimmed_name = name.trim();
            if trimmed_name.is_empty() {
                continue;
            }
            let key = normalize_path_key(&raw_path);
            if !key.is_empty() {
                out.insert(key, trimmed_name.to_string());
            }
        }
        out
    }
}

/// Normalizes a `file://` URI or raw filesystem path into a platform path string,
/// accepting Windows backslashes (`\`) inside `file://` URIs without rejecting them.
pub fn normalize_file_uri(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("file://") {
        // Pre-replace `\` with `/` in a copy for URL parsing, because `url::Url` handles
        // Windows `file://` cleanly when drive slashes are consistent, and fall back
        // manually if `to_file_path()` cannot resolve (e.g. `file://C:\foo` or Unix path on Windows).
        let replaced = trimmed.replace('\\', "/");
        if let Ok(url) = Url::parse(&replaced)
            && let Ok(path_buf) = url.to_file_path()
        {
            return Some(path_buf);
        }

        // Manual fallback for `file://C:/foo`, `file:///C:\foo`, or Unix paths when running on Windows.
        let rest = &replaced[7..];
        let decoded = percent_decode_utf8(rest);
        let candidate = if let Some(after_slash) = decoded.strip_prefix('/') {
            if is_windows_drive_prefix(after_slash) {
                after_slash.to_string()
            } else {
                decoded
            }
        } else {
            decoded
        };
        if candidate.is_empty() {
            return None;
        }
        return Some(PathBuf::from(normalize_separators(&candidate)));
    }

    Some(PathBuf::from(normalize_separators(trimmed)))
}

/// Produces a canonical comparison key for matching `projects.json` keys and `workspace_uris`.
pub fn normalize_path_key(raw: &str) -> String {
    let Some(pb) = normalize_file_uri(raw) else {
        return String::new();
    };
    let mut s = pb.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') && !is_drive_root(&s) {
        s.pop();
    }
    if cfg!(windows) || is_windows_drive_prefix(&s) {
        s.make_ascii_lowercase();
    }
    s
}

pub fn path_basename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().to_string())
}

fn is_windows_drive_prefix(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'/' || bytes[2] == b'\\')
}

fn is_drive_root(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() == 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

fn normalize_separators(s: &str) -> String {
    if cfg!(windows) || is_windows_drive_prefix(s) {
        s.replace('/', "\\")
    } else {
        s.to_string()
    }
}

fn percent_decode_utf8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
