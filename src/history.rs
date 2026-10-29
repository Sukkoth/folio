use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentSession {
    pub page: i32,
    pub scroll_y: f32,
    pub zoom_level: f32,
    pub is_scroll_view: bool,
    pub last_opened_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HistoryStore {
    pub documents: HashMap<String, DocumentSession>,
}

fn history_file_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(base.join("folio").join("history.json"))
}

pub fn canonical_key(path: &str) -> String {
    fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

pub fn load_document_session(file_path: &str) -> Option<DocumentSession> {
    let p = history_file_path()?;
    load_document_session_from(file_path, &p)
}

pub fn load_document_session_from(file_path: &str, history_path: &Path) -> Option<DocumentSession> {
    if !history_path.exists() {
        return None;
    }
    let data = fs::read_to_string(history_path).ok()?;
    let store: HistoryStore = serde_json::from_str(&data).ok()?;
    let key = canonical_key(file_path);
    store.documents.get(&key).cloned()
}

pub fn save_document_session(file_path: &str, session: DocumentSession) {
    if let Some(p) = history_file_path() {
        save_document_session_to(file_path, session, &p);
    }
}

pub fn save_document_session_to(file_path: &str, mut session: DocumentSession, history_path: &Path) {
    if session.last_opened_at == 0 {
        if let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) {
            session.last_opened_at = now.as_secs();
        }
    }

    let mut store = if history_path.exists() {
        fs::read_to_string(history_path)
            .ok()
            .and_then(|data| serde_json::from_str::<HistoryStore>(&data).ok())
            .unwrap_or_default()
    } else {
        HistoryStore::default()
    };

    let key = canonical_key(file_path);
    store.documents.insert(key, session);

    // Keep store bounded to 500 documents
    if store.documents.len() > 500 {
        let mut entries: Vec<(String, u64)> = store
            .documents
            .iter()
            .map(|(k, v)| (k.clone(), v.last_opened_at))
            .collect();
        entries.sort_by_key(|(_, ts)| *ts);
        while store.documents.len() > 500 && !entries.is_empty() {
            let (oldest_key, _) = entries.remove(0);
            store.documents.remove(&oldest_key);
        }
    }

    if let Some(parent) = history_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = fs::write(history_path, json);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecentDocument {
    pub path: String,
    pub filename: String,
    pub page: i32,
    pub last_opened_at: u64,
}

pub fn get_recent_documents(limit: usize) -> Vec<RecentDocument> {
    if let Some(p) = history_file_path() {
        get_recent_documents_from(&p, limit)
    } else {
        Vec::new()
    }
}

pub fn get_recent_documents_from(history_path: &Path, limit: usize) -> Vec<RecentDocument> {
    if !history_path.exists() {
        return Vec::new();
    }
    let data = match fs::read_to_string(history_path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let store: HistoryStore = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut list: Vec<RecentDocument> = store
        .documents
        .into_iter()
        .map(|(path, sess)| {
            let filename = Path::new(&path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            RecentDocument {
                path,
                filename,
                page: sess.page,
                last_opened_at: sess.last_opened_at,
            }
        })
        .collect();
    list.sort_by(|a, b| b.last_opened_at.cmp(&a.last_opened_at));
    list.truncate(limit);
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_session_serialization() {
        let session = DocumentSession {
            page: 14,
            scroll_y: -1280.5,
            zoom_level: 2.5,
            is_scroll_view: true,
            last_opened_at: 1729000000,
        };

        let json = serde_json::to_string(&session).unwrap();
        let deserialized: DocumentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session, deserialized);
    }

    #[test]
    fn test_history_store_roundtrip() {
        let mut store = HistoryStore::default();
        store.documents.insert(
            "/home/user/doc.pdf".to_string(),
            DocumentSession {
                page: 5,
                scroll_y: -500.0,
                zoom_level: 1.5,
                is_scroll_view: false,
                last_opened_at: 100,
            },
        );

        let json = serde_json::to_string_pretty(&store).unwrap();
        let loaded: HistoryStore = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.documents.len(), 1);
        let s = loaded.documents.get("/home/user/doc.pdf").unwrap();
        assert_eq!(s.page, 5);
        assert_eq!(s.zoom_level, 1.5);
        assert!(!s.is_scroll_view);
    }

    #[test]
    fn test_save_and_load_file_roundtrip() {
        let temp_dir = std::env::temp_dir();
        let test_history_path = temp_dir.join("folio_test_history.json");
        let _ = fs::remove_file(&test_history_path);

        let test_doc_path = "/tmp/fake_doc.pdf";
        let session = DocumentSession {
            page: 23,
            scroll_y: -2450.0,
            zoom_level: 1.8,
            is_scroll_view: true,
            last_opened_at: 0,
        };

        save_document_session_to(test_doc_path, session.clone(), &test_history_path);

        let loaded = load_document_session_from(test_doc_path, &test_history_path);
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.page, 23);
        assert_eq!(loaded.scroll_y, -2450.0);
        assert_eq!(loaded.zoom_level, 1.8);
        assert!(loaded.is_scroll_view);
        assert!(loaded.last_opened_at > 0);

        let _ = fs::remove_file(&test_history_path);
    }

    #[test]
    fn test_get_recent_documents_order() {
        let temp_dir = std::env::temp_dir();
        let test_history_path = temp_dir.join("folio_test_history_recents.json");
        let _ = fs::remove_file(&test_history_path);

        save_document_session_to(
            "/path/to/old.pdf",
            DocumentSession {
                page: 2,
                scroll_y: 0.0,
                zoom_level: 1.0,
                is_scroll_view: false,
                last_opened_at: 1000,
            },
            &test_history_path,
        );

        save_document_session_to(
            "/path/to/newest.pdf",
            DocumentSession {
                page: 15,
                scroll_y: 0.0,
                zoom_level: 1.0,
                is_scroll_view: false,
                last_opened_at: 5000,
            },
            &test_history_path,
        );

        let recents = get_recent_documents_from(&test_history_path, 10);
        assert_eq!(recents.len(), 2);
        assert_eq!(recents[0].filename, "newest.pdf");
        assert_eq!(recents[0].page, 15);
        assert_eq!(recents[1].filename, "old.pdf");

        let _ = fs::remove_file(&test_history_path);
    }
}
