//! The Python tool scripts a worker ships into the box's `tools_dir` (#207).
//!
//! A worker that spawns a Python child embeds the child's script AND every
//! module that script imports at load (`include_str!`, so they always ship
//! with the binary), and writes them next to each other before the spawn. A
//! deploy therefore never runs a new script against a stale or missing helper.
//! Shared by the stem worker (`stem_worker.py` + `win_replace.py`) and the dub
//! worker (`dub_worker.py` + its three modules).

use std::path::Path;

/// Write every `(file name, content)` pair into `tools_dir` (created when
/// missing), rewriting only a file whose on-disk content differs or cannot be
/// read. Returns the names it wrote, in order; `who` names the worker in the
/// INFO line of each write.
pub(crate) async fn materialise(
    tools_dir: &Path,
    scripts: &[(&'static str, &'static str)],
    who: &str,
) -> std::io::Result<Vec<&'static str>> {
    tokio::fs::create_dir_all(tools_dir).await?;
    let mut written = Vec::new();
    for &(name, content) in scripts {
        let path = tools_dir.join(name);
        let stale = match tokio::fs::read_to_string(&path).await {
            Ok(existing) => existing != content,
            Err(_) => true,
        };
        if stale {
            tokio::fs::write(&path, content).await?;
            tracing::info!("{who}: wrote {}", path.display());
            written.push(name);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPTS: [(&str, &str); 2] = [("worker.py", "import helper\n"), ("helper.py", "X = 1\n")];

    async fn read(dir: &Path, name: &str) -> String {
        tokio::fs::read_to_string(dir.join(name)).await.unwrap()
    }

    #[tokio::test]
    async fn a_missing_dir_and_missing_scripts_are_all_written() {
        let root = tempfile::tempdir().unwrap();
        let tools = root.path().join("tools");

        let written = materialise(&tools, &SCRIPTS, "test").await.unwrap();

        assert_eq!(written, vec!["worker.py", "helper.py"]);
        assert_eq!(read(&tools, "worker.py").await, "import helper\n");
        assert_eq!(read(&tools, "helper.py").await, "X = 1\n");
    }

    #[tokio::test]
    async fn only_a_stale_script_is_rewritten() {
        let tools = tempfile::tempdir().unwrap();
        tokio::fs::write(tools.path().join("worker.py"), "import helper\n")
            .await
            .unwrap();
        tokio::fs::write(tools.path().join("helper.py"), "X = 0\n")
            .await
            .unwrap();

        let written = materialise(tools.path(), &SCRIPTS, "test").await.unwrap();

        assert_eq!(written, vec!["helper.py"]);
        assert_eq!(read(tools.path(), "helper.py").await, "X = 1\n");
        assert_eq!(read(tools.path(), "worker.py").await, "import helper\n");
    }

    #[tokio::test]
    async fn an_up_to_date_set_writes_nothing() {
        let tools = tempfile::tempdir().unwrap();
        materialise(tools.path(), &SCRIPTS, "test").await.unwrap();

        let written = materialise(tools.path(), &SCRIPTS, "test").await.unwrap();

        assert!(written.is_empty(), "rewrote {written:?}");
    }
}
