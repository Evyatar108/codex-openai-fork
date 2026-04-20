use anyhow::Result;
use codex_state::StateRuntime;
use codex_state::state_db_path;
use std::path::Path;

pub async fn clear_memories(
    sqlite_home: &Path,
    codex_home: &Path,
    model_provider_id: &str,
) -> Result<String> {
    let state_path = state_db_path(sqlite_home);
    let mut cleared_state_db = false;
    if tokio::fs::try_exists(&state_path).await? {
        let state_db =
            StateRuntime::init(sqlite_home.to_path_buf(), model_provider_id.to_string()).await?;
        state_db.clear_memory_data().await?;
        cleared_state_db = true;
    }

    let memory_root = codex_home.join("memories");
    let removed_memory_root = match tokio::fs::remove_dir_all(&memory_root).await {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(err.into()),
    };

    let mut message = if cleared_state_db {
        format!("Cleared memory state from {}.", state_path.display())
    } else {
        format!("No state db found at {}.", state_path.display())
    };

    if removed_memory_root {
        message.push_str(&format!(" Removed {}.", memory_root.display()));
    } else {
        message.push_str(&format!(
            " No memory directory found at {}.",
            memory_root.display()
        ));
    }

    Ok(message)
}
