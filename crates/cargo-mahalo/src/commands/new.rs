use std::fs;
use std::io;
use std::path::Path;

use crate::templates::project;

pub fn run(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(name);

    if root.exists() {
        return Err(format!("Directory '{name}' already exists").into());
    }

    // Create directory structure
    let dirs = [
        format!("crates/{name}/src"),
        format!("crates/{name}_models/src"),
        format!("crates/{name}_web/src/controllers"),
        format!("crates/{name}_web/src/channels"),
    ];

    for dir in &dirs {
        fs::create_dir_all(root.join(dir))?;
    }

    // Write files
    let files: Vec<(String, String)> = vec![
        ("Cargo.toml".into(), project::workspace_cargo_toml(name)),
        (
            format!("crates/{name}/Cargo.toml"),
            project::app_cargo_toml(name),
        ),
        (
            format!("crates/{name}/src/main.rs"),
            project::app_main_rs(name),
        ),
        (
            format!("crates/{name}_models/Cargo.toml"),
            project::models_cargo_toml(name),
        ),
        (
            format!("crates/{name}_models/src/lib.rs"),
            project::models_lib_rs(),
        ),
        (
            format!("crates/{name}_web/Cargo.toml"),
            project::web_cargo_toml(name),
        ),
        (
            format!("crates/{name}_web/src/lib.rs"),
            project::web_lib_rs(),
        ),
        (
            format!("crates/{name}_web/src/router.rs"),
            project::web_router_rs(name),
        ),
        (
            format!("crates/{name}_web/src/controllers/mod.rs"),
            project::controllers_mod_rs(),
        ),
        (
            format!("crates/{name}_web/src/channels/mod.rs"),
            project::channels_mod_rs(),
        ),
    ];

    for (path, content) in &files {
        fs::write(root.join(path), content)?;
    }

    println!("Created new Mahalo project: {name}");
    println!();
    println!("  cd {name}");
    println!("  cargo run");
    println!();
    println!("Project structure:");
    print_tree(root, "", true)?;

    Ok(())
}

fn print_tree(dir: &Path, prefix: &str, is_last: bool) -> io::Result<()> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if prefix.is_empty() {
        println!("  {name}/");
    } else {
        let connector = if is_last { "└── " } else { "├── " };
        if dir.is_dir() {
            println!("  {prefix}{connector}{name}/");
        } else {
            println!("  {prefix}{connector}{name}");
        }
    }

    if dir.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        let count = entries.len();
        for (i, entry) in entries.iter().enumerate() {
            let child_prefix = if prefix.is_empty() {
                "  ".to_string()
            } else {
                let ext = if is_last { "    " } else { "│   " };
                format!("{prefix}{ext}")
            };
            print_tree(&entry.path(), &child_prefix, i == count - 1)?;
        }
    }

    Ok(())
}
