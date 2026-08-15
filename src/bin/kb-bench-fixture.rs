use kb::bench_fixture::{logical_checksum, seed_db, BenchEmbedder, DEFAULT_SEED};
use kb::components::db;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()?;
    anyhow::ensure!(status.success(), "git {:?} failed", args);
    Ok(())
}

fn create_destination_root(root: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.file_type().is_symlink() => {
            anyhow::bail!("destination is a symlink: {}", root.display())
        }
        Ok(_) => anyhow::bail!("destination already exists: {}", root.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    fs::create_dir(root).map_err(|err| match err.kind() {
        io::ErrorKind::AlreadyExists => anyhow::anyhow!("destination already exists: {}", root.display()),
        _ => err.into(),
    })?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("usage: kb-bench-fixture ROOT SIZE [SEED]"))?,
    );
    let size = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing SIZE"))?
        .parse()?;
    let seed = args
        .next()
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(DEFAULT_SEED);
    anyhow::ensure!(args.next().is_none(), "unexpected arguments");
    create_destination_root(&root)?;
    fs::create_dir_all(root.join("src"))?;
    fs::create_dir_all(root.join(".state/agent-kb"))?;
    fs::write(root.join("src/hot.rs"), "alpha\nbeta\ngamma\n")?;
    fs::write(root.join("src/support.rs"), "alpha\nbeta\ngamma\n")?;
    git(&root, &["init", "-q", "-b", "bench/architecture-latency"])?;
    git(&root, &["config", "user.email", "bench@example.invalid"])?;
    git(&root, &["config", "user.name", "kb benchmark"])?;
    git(&root, &["add", "."])?;
    git(&root, &["commit", "-qm", "fixture"])?;
    fs::write(root.join("src/hot.rs"), "alpha\nbeta\ngamma\ndirty\n")?;
    fs::write(root.join("staged.txt"), "staged\n")?;
    git(&root, &["add", "staged.txt"])?;
    fs::write(
        root.join("kb.toml"),
        "inline_verify_k = 10\n[embed]\nenabled = false\n",
    )?;
    let db_path = root.join(".state/agent-kb/agent-kb.db");
    let conn = db::open_db(&db_path)?;
    let emb = BenchEmbedder::new(seed);
    seed_db(&conn, &emb, size, seed)?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    println!(
        "{}",
        serde_json::json!({"root":root,"size":size,"seed":seed,"checksum":logical_checksum(&conn)?})
    );
    Ok(())
}
