use std::fs;
use std::path::{Path, PathBuf};

use crate::templates;

pub fn controller(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (snake, pascal) = normalize_name(name);
    let web_crate = find_web_crate()?;

    let file_path = web_crate.join(format!("src/controllers/{snake}_controller.rs"));
    if file_path.exists() {
        return Err(format!("Controller already exists: {}", file_path.display()).into());
    }

    fs::write(&file_path, templates::controller::controller_file(&pascal))?;
    println!("  Created {}", file_path.display());

    inject_module(
        &web_crate.join("src/controllers/mod.rs"),
        &format!("{snake}_controller"),
    )?;

    println!();
    println!("Add a route in router.rs:");
    println!(
        "  use crate::controllers::{snake}_controller::{pascal}Controller;"
    );
    println!(
        "  s.resources(\"/{}\", Arc::new({pascal}Controller));",
        pluralize(&snake)
    );

    Ok(())
}

pub fn channel(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (snake, pascal) = normalize_name(name);
    let web_crate = find_web_crate()?;

    let file_path = web_crate.join(format!("src/channels/{snake}_channel.rs"));
    if file_path.exists() {
        return Err(format!("Channel already exists: {}", file_path.display()).into());
    }

    fs::write(
        &file_path,
        templates::channel::channel_file(&pascal, &snake),
    )?;
    println!("  Created {}", file_path.display());

    inject_module(
        &web_crate.join("src/channels/mod.rs"),
        &format!("{snake}_channel"),
    )?;

    println!();
    println!("Register in your ChannelRouter:");
    println!(
        "  use crate::channels::{snake}_channel::{pascal}Channel;"
    );
    println!(
        "  channel_router.channel(\"{snake}:*\", Arc::new({pascal}Channel));"
    );

    Ok(())
}

pub fn resource(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (snake, pascal) = normalize_name(name);
    let web_crate = find_web_crate()?;
    let plural = pluralize(&snake);

    // Generate controller file
    let file_path = web_crate.join(format!("src/controllers/{snake}_controller.rs"));
    if file_path.exists() {
        return Err(format!("Controller already exists: {}", file_path.display()).into());
    }

    fs::write(&file_path, templates::controller::controller_file(&pascal))?;
    println!("  Created {}", file_path.display());

    // Inject into controllers/mod.rs
    inject_module(
        &web_crate.join("src/controllers/mod.rs"),
        &format!("{snake}_controller"),
    )?;

    // Inject import into router.rs
    let router_path = web_crate.join("src/router.rs");
    let import_line =
        format!("use crate::controllers::{snake}_controller::{pascal}Controller;");
    let route_line = format!(
        "        .scope(\"/{plural}\", &[], |s| {{ s.resources(\"/{plural}\", std::sync::Arc::new({pascal}Controller)); }})"
    );

    inject_router(&router_path, &import_line, &route_line, &snake)?;

    println!();
    println!("Resource {pascal} generated:");
    println!("  Controller: src/controllers/{snake}_controller.rs");
    println!("  Routes:     /{plural} (index, show, create, update, delete)");

    Ok(())
}

/// Find the *_web crate directory by scanning crates/
fn find_web_crate() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let crates_dir = Path::new("crates");
    if !crates_dir.exists() {
        return Err(
            "No 'crates/' directory found. Are you in a Mahalo project root?".into(),
        );
    }

    for entry in fs::read_dir(crates_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with("_web") && entry.path().is_dir() {
            return Ok(entry.path());
        }
    }

    Err("No *_web crate found in crates/. Are you in a Mahalo project root?".into())
}

/// Inject `pub mod <module>;` into a mod.rs file after `// mahalo:modules` marker
fn inject_module(
    mod_path: &Path,
    module_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = fs::read_to_string(mod_path)?;
    let mod_line = format!("pub mod {module_name};");

    if content.contains(&mod_line) {
        println!("  Skipped {} (already has {mod_line})", mod_path.display());
        return Ok(());
    }

    let marker = "// mahalo:modules";
    let new_content = if content.contains(marker) {
        content.replacen(marker, &format!("{marker}\n{mod_line}"), 1)
    } else {
        // Fallback: append
        format!("{content}{mod_line}\n")
    };

    fs::write(mod_path, new_content)?;
    println!("  Modified {}", mod_path.display());
    Ok(())
}

/// Inject import and route into router.rs
fn inject_router(
    router_path: &Path,
    import_line: &str,
    route_line: &str,
    snake: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let content = fs::read_to_string(router_path)?;

    // Check for duplicate
    if content.contains(&format!("{snake}_controller")) {
        println!(
            "  Skipped {} (already has {snake}_controller route)",
            router_path.display()
        );
        return Ok(());
    }

    let imports_marker = "// mahalo:imports";
    let routes_marker = "// mahalo:routes";

    if !content.contains(imports_marker) || !content.contains(routes_marker) {
        println!("  Could not find marker comments in router.rs.");
        println!("  Add manually:");
        println!("    {import_line}");
        println!("    {route_line}");
        return Ok(());
    }

    let content =
        content.replacen(imports_marker, &format!("{imports_marker}\n{import_line}"), 1);
    let content =
        content.replacen(routes_marker, &format!("{routes_marker}\n{route_line}"), 1);

    fs::write(router_path, content)?;
    println!("  Modified {}", router_path.display());
    Ok(())
}

/// Convert input name to (snake_case, PascalCase)
fn normalize_name(name: &str) -> (String, String) {
    if name.contains('_') || name.chars().all(|c| c.is_lowercase() || c == '_') {
        // Already snake_case
        let snake = name.to_lowercase();
        let pascal = snake
            .split('_')
            .map(|part| {
                let mut chars = part.chars();
                match chars.next() {
                    Some(c) => {
                        let upper: String = c.to_uppercase().collect();
                        format!("{upper}{}", chars.collect::<String>())
                    }
                    None => String::new(),
                }
            })
            .collect();
        (snake, pascal)
    } else {
        // PascalCase input
        let mut snake = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_uppercase() && i > 0 {
                snake.push('_');
            }
            snake.push(c.to_ascii_lowercase());
        }
        let pascal = name.to_string();
        (snake, pascal)
    }
}

/// Naive pluralization: append "s"
fn pluralize(s: &str) -> String {
    format!("{s}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_pascal() {
        assert_eq!(normalize_name("Room"), ("room".into(), "Room".into()));
        assert_eq!(
            normalize_name("RoomMessage"),
            ("room_message".into(), "RoomMessage".into())
        );
    }

    #[test]
    fn test_normalize_snake() {
        assert_eq!(normalize_name("room"), ("room".into(), "Room".into()));
        assert_eq!(
            normalize_name("room_message"),
            ("room_message".into(), "RoomMessage".into())
        );
    }

    #[test]
    fn test_pluralize() {
        assert_eq!(pluralize("room"), "rooms");
        assert_eq!(pluralize("post"), "posts");
    }
}
