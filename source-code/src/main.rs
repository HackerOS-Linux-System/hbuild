use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::os::unix::process::ExitStatusExt;

use indicatif::{ProgressBar, ProgressStyle};
use lexopt::prelude::*;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
struct Metadata {
    name: String,
    version: String,
    authors: Option<String>,
    license: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Description {
    summary: String,
    long: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Specs {
    languages: Vec<String>,
    dependencies: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Runtime {
    priority: Option<String>,
    #[serde(rename = "auto-restart")]
    auto_restart: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Config {
    metadata: Metadata,
    description: Description,
    specs: Specs,
    runtime: Option<Runtime>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut parser = lexopt::Parser::from_env();
    let subcommand: String = match parser.next()? {
        Some(Value(val)) => val.string()?,
        _ => {
            print_help();
            return Ok(());
        }
    };

    let folder: String = match parser.next()? {
        Some(Value(val)) => val.string()?,
        _ => {
            eprintln!("{}", "Missing folder argument".red().bold());
            print_help();
            return Ok(());
        }
    };

    let project_path = PathBuf::from(&folder);
    if !project_path.exists() {
        eprintln!("{}", format!("Folder '{}' does not exist", folder).red().bold());
        return Ok(());
    }

    match subcommand.as_str() {
        "setup" => setup(&project_path)?,
        "make" => make(&project_path)?,
        "clean" => clean(&project_path)?,
        "remake" => {
            clean(&project_path)?;
            make(&project_path)?;
        }
        _ => {
            eprintln!("{}", "Unknown subcommand".red().bold());
            print_help();
        }
    }

    Ok(())
}

fn print_help() {
    println!("{}", "hbuild - Modern build tool for HackerOS".green().bold());
    println!("Usage: hbuild <subcommand> <folder>");
    println!("Subcommands:");
    println!("  setup   - Initialize project configuration");
    println!("  make    - Build the project");
    println!("  clean   - Clean build artifacts");
    println!("  remake  - Clean and rebuild");
}

fn find_config_file(path: &Path) -> Option<(PathBuf, String)> {
    let possible_files = vec![
        ("hbuild.config", "hk"),
        ("hbuilt.config", "toml"),
        ("hbuily.config", "yaml"),
        ("hbuilj.config", "json"),
        ("hbuilh.config", "hcl"),
    ];

    for (filename, format) in possible_files {
        let config_path = path.join(filename);
        if config_path.exists() {
            return Some((config_path, format.to_string()));
        }
    }
    None
}

fn parse_config(config_path: &Path, format: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let content = fs::read_to_string(config_path)?;

    match format {
        "hk" => parse_hk(&content),
        "toml" => toml::from_str(&content).map_err(|e| e.into()),
        "yaml" => serde_yaml::from_str(&content).map_err(|e| e.into()),
        "json" => serde_json::from_str(&content).map_err(|e| e.into()),
        "hcl" => hcl::from_str(&content).map_err(|e| e.into()),
        _ => Err("Unknown format".into()),
    }
}

fn parse_hk(content: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let mut metadata = Metadata {
        name: String::new(),
        version: String::new(),
        authors: None,
        license: None,
    };
    let mut description = Description {
        summary: String::new(),
        long: Vec::new(),
    };
    let mut specs = Specs {
        languages: Vec::new(),
        dependencies: HashMap::new(),
    };
    let mut runtime: Option<Runtime> = None;

    let mut current_section = "";
    let mut in_deps = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('!') {
            continue; // comment
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            current_section = &trimmed[1..trimmed.len() - 1];
            in_deps = false;
            continue;
        }
        if !(trimmed.starts_with("->") || (in_deps && trimmed.starts_with("-->"))) {
            continue;
        }

        let prefix = if trimmed.starts_with("-->") { "-->" } else { "->" };
        let start_idx = prefix.len();
        let rest = trimmed[start_idx..].trim();

        let parts: Vec<&str> = rest.splitn(2, "=>").map(|s| s.trim()).collect();
        let key = parts[0];
        let value = if parts.len() == 2 { Some(parts[1]) } else { None };

        match current_section {
            "metadata" => match key {
                "name" => if let Some(v) = value { metadata.name = v.to_string() },
                "version" => if let Some(v) = value { metadata.version = v.to_string() },
                "authors" => if let Some(v) = value { metadata.authors = Some(v.to_string()) },
                "license" => if let Some(v) = value { metadata.license = Some(v.to_string()) },
                _ => {}
            },
            "description" => match key {
                "summary" => if let Some(v) = value { description.summary = v.to_string() },
                "long" => if let Some(v) = value { description.long.push(v.to_string()) },
                _ => {}
            },
            "specs" => {
                if prefix == "->" {
                    if let Some(_) = value {
                        // Unexpected => in top level
                    } else {
                        if key == "dependencies" {
                            in_deps = true;
                        } else {
                            specs.languages.push(key.to_string());
                        }
                    }
                } else if prefix == "-->" && in_deps {
                    if let Some(v) = value {
                        specs.dependencies.insert(key.to_string(), v.to_string());
                    }
                }
            }
            "runtime" => {
                if runtime.is_none() {
                    runtime = Some(Runtime {
                        priority: None,
                        auto_restart: None,
                    });
                }
                if let Some(r) = runtime.as_mut() {
                    if let Some(v) = value {
                        match key {
                            "priority" => r.priority = Some(v.to_string()),
                            "auto-restart" => r.auto_restart = Some(v == "true"),
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(Config {
        metadata,
       description,
       specs,
       runtime,
    })
}

fn setup(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "Setting up project...".blue().bold());

    let pb = ProgressBar::new(100);
    pb.set_style(ProgressStyle::default_bar()
    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
    .unwrap()
    .progress_chars("#>-"));

    for i in 0..100 {
        pb.set_position(i);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let config_path = path.join("hbuild.config");
    if config_path.exists() {
        println!("{}", "Config already exists".yellow().bold());
        return Ok(());
    }

    let mut file = File::create(&config_path)?;
    file.write_all(b"! Example hbuild.config\n[metadata]\n-> name => MyProject\n-> version => 0.1.0\n")?;

    println!("{}", "Setup complete!".green().bold());
    Ok(())
}

fn make(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some((config_path, format)) = find_config_file(path) {
        match parse_config(&config_path, &format) {
            Ok(config) => {
                println!("{}", format!("Building project: {}", config.metadata.name).blue().bold());

                let pb = ProgressBar::new(100);
                pb.set_style(ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta}) {msg}")
                .unwrap()
                .progress_chars("#>-"));

                // Install dependencies (simulated)
                pb.set_message("Installing dependencies...");
                for _ in 0..50 {
                    pb.inc(1);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }

                // Build based on languages
                pb.set_message("Building...");
                for lang in &config.specs.languages {
                    pb.println(format!("Building for {}...", lang).cyan().to_string());
                    let build_result = match lang.as_str() {
                        "rust" => {
                            Command::new("cargo").arg("build").current_dir(path).status()
                        }
                        "c++" | "c" => {
                            Command::new("cmake").arg(".").current_dir(path).status()?;
                            Command::new("make").current_dir(path).status()
                        }
                        "odin" => {
                            Command::new("odin").arg("build").arg(".").current_dir(path).status()
                        }
                        "python" => {
                            if path.join("requirements.txt").exists() {
                                Command::new("pip").arg("install").arg("-r").arg("requirements.txt").current_dir(path).status()
                            } else {
                                Ok(std::process::ExitStatus::from_raw(0))
                            }
                        }
                        "crystal" => {
                            Command::new("crystal").arg("build").arg("main.cr").current_dir(path).status()
                        }
                        "go" => {
                            Command::new("go").arg("build").current_dir(path).status()
                        }
                        "vala" => {
                            Command::new("valac").args(&["--pkg", "gio-2.0", "main.vala"]).current_dir(path).status() // example
                        }
                        _ => {
                            pb.println(format!("Unsupported language: {}", lang).yellow().to_string());
                            Ok(std::process::ExitStatus::from_raw(0))
                        }
                    };

                    if let Ok(status) = build_result {
                        if status.success() {
                            // success
                        } else {
                            eprintln!("{}", format!("Build failed for {}", lang).red().bold());
                        }
                    } else if let Err(e) = build_result {
                        eprintln!("{}", format!("Failed to run build command for {}: {}", lang, e).red().bold());
                    }
                }

                for _ in 50..100 {
                    pb.inc(1);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }

                pb.finish_with_message("Build complete!");
                println!("{}", "Build successful!".green().bold());
            }
            Err(e) => {
                eprintln!("{}", format!("Config parse error: {}", e).red().bold());
            }
        }
    } else {
        eprintln!("{}", "No config file found".red().bold());
    }
    Ok(())
}

fn clean(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "Cleaning project...".blue().bold());

    let pb = ProgressBar::new(100);
    pb.set_style(ProgressStyle::default_bar()
    .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
    .unwrap()
    .progress_chars("#>-"));

    // Clean based on project type
    if path.join("Cargo.toml").exists() {
        let status = Command::new("cargo").arg("clean").current_dir(path).status()?;
        if !status.success() {
            eprintln!("{}", "Cargo clean failed".red().bold());
        }
    }
    if path.join("Makefile").exists() {
        let status = Command::new("make").arg("clean").current_dir(path).status()?;
        if !status.success() {
            eprintln!("{}", "Make clean failed".red().bold());
        }
    }
    // Add more for other build systems

    for i in 0..100 {
        pb.set_position(i);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    println!("{}", "Clean complete!".green().bold());
    Ok(())
}
