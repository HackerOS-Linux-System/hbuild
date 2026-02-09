use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use lexopt::prelude::*;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use hk_parser::{HkConfig, HkValue, parse_hk};

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
    long: String,
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
        "hk" => from_hk(parse_hk(&content)?),
        "toml" => toml::from_str(&content).map_err(|e| e.into()),
        "yaml" => serde_yaml::from_str(&content).map_err(|e| e.into()),
        "json" => serde_json::from_str(&content).map_err(|e| e.into()),
        "hcl" => hcl::from_str(&content).map_err(|e| e.into()),
        _ => Err("Unknown format".into()),
    }
}

fn from_hk(hk: HkConfig) -> Result<Config, Box<dyn std::error::Error>> {
    fn get_map(hk: &HkConfig, section: &str) -> Result<HashMap<String, HkValue>, Box<dyn std::error::Error>> {
        if let Some(HkValue::Map(m)) = hk.get(section) {
            Ok(m.clone())
        } else {
            Err(format!("Missing or invalid section {}", section).into())
        }
    }

    fn get_string(map: &HashMap<String, HkValue>, key: &str) -> Result<String, Box<dyn std::error::Error>> {
        if let Some(HkValue::String(s)) = map.get(key) {
            Ok(s.clone())
        } else {
            Err(format!("Missing or invalid key {}", key).into())
        }
    }

    fn get_opt_string(map: &HashMap<String, HkValue>, key: &str) -> Option<String> {
        map.get(key).and_then(|v| if let HkValue::String(s) = v { Some(s.clone()) } else { None })
    }

    fn get_opt_bool(map: &HashMap<String, HkValue>, key: &str) -> Option<bool> {
        get_opt_string(map, key).and_then(|s| s.parse::<bool>().ok())
    }

    let meta_map = get_map(&hk, "metadata")?;

    let metadata = Metadata {
        name: get_string(&meta_map, "name")?,
        version: get_string(&meta_map, "version")?,
        authors: get_opt_string(&meta_map, "authors"),
        license: get_opt_string(&meta_map, "license"),
    };

    let desc_map = get_map(&hk, "description")?;

    let description = Description {
        summary: get_string(&desc_map, "summary")?,
        long: get_string(&desc_map, "long")?,
    };

    let specs_map = get_map(&hk, "specs")?;

    let mut languages: Vec<String> = Vec::new();
    let mut dependencies: HashMap<String, String> = HashMap::new();

    for (k, v) in specs_map {
        if k == "dependencies" {
            if let HkValue::Map(sub) = v {
                for (sk, sv) in sub {
                    if let HkValue::String(ss) = sv {
                        dependencies.insert(sk, ss);
                    }
                }
            }
        } else if let HkValue::String(_) = v {
            languages.push(k);
        }
    }

    let specs = Specs {
        languages,
        dependencies,
    };

    let runtime = if let Ok(run_map) = get_map(&hk, "runtime") {
        Some(Runtime {
            priority: get_opt_string(&run_map, "priority"),
            auto_restart: get_opt_bool(&run_map, "auto-restart"),
        })
    } else {
        None
    };

    Ok(Config {
        metadata,
        description,
        specs,
        runtime,
    })
}

fn setup(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", "Setting up project...".blue().bold());

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

                // Install dependencies (simulated)
                println!("{}", "Installing dependencies...".cyan());

                // Build based on languages
                println!("{}", "Building...".cyan());
                for lang in &config.specs.languages {
                    println!("{}", format!("Building for {}...", lang).cyan());
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
                            println!("{}", format!("Unsupported language: {}", lang).yellow());
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

                println!("{}", "Build complete!".green().bold());
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

    println!("{}", "Clean complete!".green().bold());
    Ok(())
        }
