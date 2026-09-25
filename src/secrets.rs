//! Secret access is a Core service. The plugin receives only a namespaced reference and never
//! stores credentials beside its SQLite database or emits them in protocol events.

pub trait SecretStore: Send + Sync {
    fn has(&self, profile_id: i64) -> Result<bool, String>;
    fn set(&self, profile_id: i64, value: &str) -> Result<(), String>;
    fn delete(&self, profile_id: i64) -> Result<(), String>;
}
