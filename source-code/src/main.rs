use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use lexopt::prelude::*;
use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};
use hk_parser::{HkConfig, HkValue, parse_hk, resolve_interpolations};
use rayon::prelude::*;
use git2::{Repository, FetchOptions};
use glob::glob;
use dirs::home_dir;
use num_cpus;
use ctrlc;
use pkg_config;
use indexmap::IndexMap;
use std::os::unix::process::ExitStatusExt;

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
struct Build {
    target: String,
    sources: Vec<String>,
    include_dirs: Vec<String>,
    compiler: String,
    standard: String,
    optimize: String,
    cflags: Option<String>,
    ldflags: Option<String>,
    lib_dirs: Option<Vec<String>>,
    libs: Option<Vec<String>>,
    pkg_dependencies: Option<Vec<String>>,
    build_type: String, // "executable", "shared", "static"
    native: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
struct BuildState {
    hashes: HashMap<PathBuf, String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct HBuildConfig {
    metadata: Metadata,
    description: Description,
    specs: Specs,
    runtime: Option<Runtime>,
    build: Option<Build>,
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let children: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let children_clone = children.clone();
    ctrlc::set_handler(move || {
        let guards = children_clone.lock().unwrap();
        for &pid in guards.iter() {
            let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        std::process::exit(1);
    })?;

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
        "make" => make(&project_path, &children)?,
        "clean" => clean(&project_path)?,
        "remake" => {
            clean(&project_path)?;
            make(&project_path, &children)?;
        }
        "install" => install(&project_path)?,
        _ => {
            eprintln!("{}", "Unknown subcommand".red().bold());
            print_help();
        }
    }
    Ok(())
}

fn print_help() {
    println!("{}", "hbuild - Modern build tool for HackerOS (Linux only)".green().bold());
    println!("Usage: hbuild <subcommand> <folder>");
    println!("Subcommands:");
    println!(" setup - Initialize project configuration");
    println!(" make - Build the project");
    println!(" clean - Clean build artifacts");
    println!(" remake - Clean and rebuild");
    println!(" install - Install built artifacts to system paths");
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

fn parse_config(config_path: &Path, format: &str) -> Result<HBuildConfig, Box<dyn std::error::Error + Send + Sync>> {
    let content = fs::read_to_string(config_path)?;
    let config = match format {
        "hk" => {
            let mut hk = parse_hk(&content)?;
            resolve_interpolations(&mut hk)?;
            from_hk(hk)?
        }
        "toml" => toml::from_str::<HBuildConfig>(&content)?,
        "yaml" => serde_yaml::from_str::<HBuildConfig>(&content)?,
        "json" => serde_json::from_str::<HBuildConfig>(&content)?,
        "hcl" => hcl::from_str::<HBuildConfig>(&content)?,
        _ => return Err("Unknown format".into()),
    };
    Ok(config)
}

fn from_hk(hk: HkConfig) -> Result<HBuildConfig, Box<dyn std::error::Error + Send + Sync>> {
    fn get_map(hk: &HkConfig, section: &str) -> Result<IndexMap<String, HkValue>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(HkValue::Map(m)) = hk.get(section) {
            Ok(m.clone())
        } else {
            Err(format!("Missing or invalid section {}", section).into())
        }
    }
    fn get_string(map: &IndexMap<String, HkValue>, key: &str) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        map.get(key).and_then(|v| v.as_string().ok()).ok_or(format!("Missing or invalid key {}", key).into())
    }
    fn get_opt_string(map: &IndexMap<String, HkValue>, key: &str) -> Option<String> {
        map.get(key).and_then(|v| v.as_string().ok())
    }
    fn get_opt_bool(map: &IndexMap<String, HkValue>, key: &str) -> Option<bool> {
        map.get(key).and_then(|v| v.as_bool().ok())
    }
    fn get_vec_string(map: &IndexMap<String, HkValue>, key: &str) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(HkValue::Array(a)) = map.get(key) {
            a.iter().map(|v| v.as_string()).collect::<Result<Vec<_>, _>>().map_err(|e| e.into())
        } else {
            Err(format!("Missing or invalid array key {}", key).into())
        }
    }
    fn get_opt_vec_string(map: &IndexMap<String, HkValue>, key: &str) -> Option<Vec<String>> {
        map.get(key).and_then(|v| if let HkValue::Array(a) = v { Some(a.iter().filter_map(|vv| vv.as_string().ok()).collect()) } else { None })
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
    for (k, v) in &specs_map {
        if k == "dependencies" {
            if let HkValue::Map(sub) = v {
                for (sk, sv) in sub {
                    if let Ok(ss) = sv.as_string() {
                        dependencies.insert(sk.clone(), ss);
                    }
                }
            }
        } else if let HkValue::String(_) = v {
            languages.push(k.clone());
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
    let build = if let Ok(build_map) = get_map(&hk, "build") {
        Some(Build {
            target: get_string(&build_map, "target")?,
             sources: get_vec_string(&build_map, "sources")?,
             include_dirs: get_vec_string(&build_map, "include_dirs")?,
             compiler: get_string(&build_map, "compiler")?,
             standard: get_string(&build_map, "standard")?,
             optimize: get_string(&build_map, "optimize")?,
             cflags: get_opt_string(&build_map, "cflags"),
             ldflags: get_opt_string(&build_map, "ldflags"),
             lib_dirs: get_opt_vec_string(&build_map, "lib_dirs"),
             libs: get_opt_vec_string(&build_map, "libs"),
             pkg_dependencies: get_opt_vec_string(&build_map, "pkg_dependencies"),
             build_type: get_string(&build_map, "build_type")?,
             native: get_opt_bool(&build_map, "native"),
        })
    } else {
        None
    };
    Ok(HBuildConfig {
        metadata,
       description,
       specs,
       runtime,
       build,
    })
}

fn setup(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("{}", "Setting up project...".blue().bold());
    let config_path = path.join("hbuild.config");
    if config_path.exists() {
        println!("{}", "Config already exists".yellow().bold());
        return Ok(());
    }
    let mut file = File::create(&config_path)?;
    let example = r#"
    [build]
    -> target => "my_app"
    -> sources => ["src/*.cpp"]
    -> include_dirs => ["include"]
    -> compiler => "g++"
    -> standard => "c++20"
    -> optimize => "O3"
    -> build_type => "executable"
    -> native => true
    -> pkg_dependencies => ["glib-2.0"]
    "#;
    file.write_all(example.as_bytes())?;
    println!("{}", "Setup complete!".green().bold());
    Ok(())
}

fn install_deps(config: &HBuildConfig, path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let home = home_dir().ok_or("Cannot find home directory")?;
    let cache = home.join(".hbuild/cache");
    fs::create_dir_all(&cache)?;
    for (name, url_or_ver) in &config.specs.dependencies {
        if url_or_ver.starts_with("https://") && url_or_ver.ends_with(".git") || url_or_ver.starts_with("git://") {
            let dep_dir = cache.join(name);
            if !dep_dir.exists() {
                Repository::clone(url_or_ver, &dep_dir)?;
            } else {
                let repo = Repository::open(&dep_dir)?;
                let mut remote = repo.find_remote("origin")?;
                let mut fetch_options = FetchOptions::new();
                remote.fetch(&["master"], Some(&mut fetch_options), None)?;
            }
            if find_config_file(&dep_dir).is_some() {
                make(&dep_dir, &Arc::new(Mutex::new(Vec::new())))?;
            }
        } else if config.specs.languages.contains(&"rust".to_string()) {
            let status = Command::new("cargo")
            .args(["add", name, "--vers", url_or_ver])
            .current_dir(path)
            .status()?;
            if !status.success() {
                eprintln!("{}", format!("Failed to add Rust dependency {}", name).red().bold());
            }
        }
    }
    Ok(())
}

fn needs_recompile(
    file: &PathBuf,
    obj: &PathBuf,
    deps: &HashMap<PathBuf, HashSet<PathBuf>>,
    cache: &mut HashMap<PathBuf, bool>,
    obj_mtime: SystemTime,
) -> bool {
    if let Some(&res) = cache.get(file) {
        return res;
    }
    let file_mtime = match file.metadata() {
        Ok(meta) => meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        Err(_) => return true,
    };
    let res = if !obj.exists() || file_mtime > obj_mtime {
        true
    } else {
        false
    };
    if res {
        cache.insert(file.clone(), true);
        return true;
    }
    if let Some(d) = deps.get(file) {
        for dep in d {
            if needs_recompile(dep, obj, deps, cache, obj_mtime) {
                cache.insert(file.clone(), true);
                return true;
            }
        }
    }
    cache.insert(file.clone(), false);
    false
}

fn get_dependencies(compiler: &str, file: &Path, include_flags: &str) -> Result<HashSet<PathBuf>, Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new(compiler)
    .arg("-MM")
    .arg(file.to_str().unwrap())
    .args(include_flags.split_whitespace())
    .output()?;
    if !output.status.success() {
        return Err(format!("Failed to get dependencies for {}", file.display()).into());
    }
    let dep_str = String::from_utf8_lossy(&output.stdout);
    let deps: Vec<&str> = dep_str.split(':').nth(1).unwrap_or("").split_whitespace().collect();
    let mut dep_set = HashSet::new();
    for d in deps {
        let dep_path = PathBuf::from(d);
        if dep_path.exists() {
            dep_set.insert(dep_path.canonicalize()?);
        }
    }
    Ok(dep_set)
}

fn compile_c_cpp(config: &HBuildConfig, path: &Path, children: &Arc<Mutex<Vec<u32>>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let build = config.build.as_ref().ok_or("No build section for C/C++")?;
    let compiler = &build.compiler;
    let std_flag = format!("-std={}", build.standard);
    let opt_flag = format!("-{}", build.optimize);
    let mut cflags = build.cflags.clone().unwrap_or_default();
    let mut ldflags = build.ldflags.clone().unwrap_or_default();
    let include_dirs: Vec<PathBuf> = build.include_dirs.iter().map(|d| path.join(d)).collect();
    let mut include_flags = include_dirs.iter().map(|d| format!("-I{}", d.display())).collect::<Vec<_>>().join(" ");
    let lib_dirs = build.lib_dirs.clone().unwrap_or_default();
    let lib_dir_flags = lib_dirs.iter().map(|d| format!("-L{}", path.join(d).display())).collect::<Vec<_>>().join(" ");
    let libs = build.libs.clone().unwrap_or_default();
    let lib_flags = libs.iter().map(|l| format!("-l{}", l)).collect::<Vec<_>>().join(" ");
    let pkg_deps = build.pkg_dependencies.clone().unwrap_or_default();

    // Pkg-config
    for pkg in &pkg_deps {
        if let Ok(lib) = pkg_config::probe_library(pkg) {
            for path in &lib.include_paths {
                include_flags.push_str(&format!(" -I{}", path.display()));
            }
            for (key, val) in &lib.defines {
                if let Some(val) = val {
                    cflags.push_str(&format!(" -D{}={}", key, val));
                } else {
                    cflags.push_str(&format!(" -D{}", key));
                }
            }
            for path in &lib.link_paths {
                ldflags.push_str(&format!(" -L{}", path.display()));
            }
            for l in &lib.libs {
                ldflags.push_str(&format!(" -l{}", l));
            }
        } else {
            eprintln!("{}", format!("Pkg-config failed for {}", pkg).yellow());
        }
    }

    // Native
    if build.native.unwrap_or(false) {
        cflags.push_str(" -march=native");
    }

    // Parallelism
    let num_threads = num_cpus::get();
    rayon::ThreadPoolBuilder::new().num_threads(num_threads).build_global()?;

    // Scan sources
    let mut sources: Vec<PathBuf> = vec![];
    for pattern in &build.sources {
        for entry in glob(path.join(pattern).to_str().ok_or("Invalid path")?)? {
            sources.push(entry?);
        }
    }

    // Build directory
    let build_dir = path.join("build");
    fs::create_dir_all(&build_dir)?;

    // Build dependency graph
    let mut deps: HashMap<PathBuf, HashSet<PathBuf>> = HashMap::new();
    for src in &sources {
        let src_deps = get_dependencies(compiler, src, &include_flags)?;
        for dep in &src_deps {
            if !deps.contains_key(dep) {
                if dep.extension().map_or(false, |e| e == "h" || e == "hpp") {
                    deps.insert(dep.clone(), get_dependencies(compiler, dep, &include_flags)?);
                }
            }
        }
        deps.insert(src.clone(), src_deps);
    }

    // Determine which sources need recompilation
    let mut to_compile: Vec<PathBuf> = vec![];
    for src in &sources {
        let obj = build_dir.join(src.file_name().unwrap()).with_extension("o");
        let obj_mtime = if obj.exists() {
            obj.metadata()?.modified()?
        } else {
            SystemTime::UNIX_EPOCH
        };
        let mut cache: HashMap<PathBuf, bool> = HashMap::new();
        if needs_recompile(src, &obj, &deps, &mut cache, obj_mtime) {
            to_compile.push(src.clone());
        }
    }

    // Parallel compilation
    to_compile.par_iter().try_for_each_init(
        || children.clone(),
                                            |children_arc, src| -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
                                                let obj = build_dir.join(src.file_name().unwrap()).with_extension("o");
                                                let mut compile_flags = format!("{} {} {} {} -c {} -o {}", std_flag, opt_flag, cflags, include_flags, src.display(), obj.display());
                                                if build.build_type == "shared" {
                                                    compile_flags.push_str(" -fPIC");
                                                }
                                                // FIXED: Removed 'mut' as child is consumed by wait_with_output
                                                let child = Command::new(compiler)
                                                .args(compile_flags.split_whitespace())
                                                .current_dir(path)
                                                .stdout(Stdio::piped())
                                                .stderr(Stdio::piped())
                                                .spawn()?;

                                                // FIXED: Capture ID before moving child into wait_with_output
                                                let child_id = child.id();
                                                {
                                                    let mut guards = children_arc.lock().unwrap();
                                                    guards.push(child_id);
                                                }

                                                let output = child.wait_with_output()?;
                                                if !output.status.success() {
                                                    eprintln!("{}", String::from_utf8_lossy(&output.stderr).red());
                                                    return Err("Compilation failed".into());
                                                }
                                                {
                                                    let mut guards = children_arc.lock().unwrap();
                                                    // FIXED: Use the captured ID
                                                    guards.retain(|&p| p != child_id);
                                                }
                                                Ok(())
                                            },
    )?;

    // Check if linking is needed
    // FIXED: Moved path extension logic here to avoid re-assigning and ensure timestamps check correct file
    let mut target_path = path.join(&build.target);
    if build.build_type == "shared" {
        target_path = target_path.with_extension("so");
    } else if build.build_type == "static" {
        target_path = target_path.with_extension("a");
    }

    let mut need_link = !target_path.exists() || !to_compile.is_empty();
    if !need_link {
        let exe_mtime = target_path.metadata()?.modified()?;
        for src in &sources {
            let obj = build_dir.join(src.file_name().unwrap()).with_extension("o");
            if obj.exists() && obj.metadata()?.modified()? > exe_mtime {
                need_link = true;
                break;
            }
        }
    }

    if need_link {
        let objs: String = sources.iter().map(|s| build_dir.join(s.file_name().unwrap()).with_extension("o").display().to_string()).collect::<Vec<_>>().join(" ");

        if build.build_type == "static" {
            // Use ar for static lib
            let status = Command::new("ar")
            .args(["rcs", target_path.to_str().unwrap()])
            .args(objs.split_whitespace())
            .current_dir(path)
            .status()?;
            if !status.success() {
                return Err("Archiving failed".into());
            }
            return Ok(());
        }

        // Shared or Executable
        // FIXED: target_path is already corrected above, so format uses correct extension
        let mut link_cmd = format!("{} {} {} {} {} -o {} {}", compiler, opt_flag, ldflags, lib_dir_flags, lib_flags, target_path.display(), objs);
        if build.build_type == "shared" {
            link_cmd.push_str(" -shared");
        }

        // FIXED: Removed 'mut'
        let child = Command::new(compiler)
        .args(link_cmd.split_whitespace())
        .current_dir(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

        // FIXED: Capture ID before moving child
        let child_id = child.id();
        {
            let mut guards = children.lock().unwrap();
            guards.push(child_id);
        }

        let output = child.wait_with_output()?;
        if !output.status.success() {
            eprintln!("{}", String::from_utf8_lossy(&output.stderr).red());
            return Err("Linking failed".into());
        }
        {
            let mut guards = children.lock().unwrap();
            // FIXED: Use captured ID
            guards.retain(|&p| p != child_id);
        }
    }
    Ok(())
}

fn make(path: &Path, children: &Arc<Mutex<Vec<u32>>>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some((config_path, format)) = find_config_file(path) {
        let config = parse_config(&config_path, &format)?;
        println!("{}", format!("Building project: {}", config.metadata.name).blue().bold());
        install_deps(&config, path)?;
        println!("{}", "Building...".cyan());
        for lang in &config.specs.languages {
            println!("{}", format!("Building for {}...", lang).cyan());
            let build_result = match lang.as_str() {
                "rust" => Command::new("cargo").arg("build").current_dir(path).status(),
                "c" | "c++" => {
                    compile_c_cpp(&config, path, children)?;
                    Ok(ExitStatusExt::from_raw(0))
                }
                "odin" => Command::new("odin").arg("build").arg(".").current_dir(path).status(),
                "python" => {
                    if path.join("requirements.txt").exists() {
                        Command::new("pip").arg("install").arg("-r").arg("requirements.txt").current_dir(path).status()
                    } else {
                        Ok(ExitStatusExt::from_raw(0))
                    }
                }
                "crystal" => Command::new("crystal").arg("build").arg("main.cr").current_dir(path).status(),
                "go" => Command::new("go").arg("build").current_dir(path).status(),
                "vala" => Command::new("valac").args(&["--pkg", "gio-2.0", "main.vala"]).current_dir(path).status(),
                _ => {
                    println!("{}", format!("Unsupported language: {}", lang).yellow());
                    Ok(ExitStatusExt::from_raw(0))
                }
            };
            if let Ok(status) = build_result {
                if !status.success() {
                    eprintln!("{}", format!("Build failed for {}", lang).red().bold());
                }
            } else if let Err(e) = build_result {
                eprintln!("{}", format!("Failed to run build command for {}: {}", lang, e).red().bold());
            }
        }
        println!("{}", "Build complete!".green().bold());
    } else {
        eprintln!("{}", "No config file found".red().bold());
    }
    Ok(())
}

fn clean(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("{}", "Cleaning project...".blue().bold());
    let build_dir = path.join("build");
    if build_dir.exists() {
        fs::remove_dir_all(&build_dir)?;
    }
    if path.join("Cargo.toml").exists() {
        Command::new("cargo").arg("clean").current_dir(path).status()?;
    }
    println!("{}", "Clean complete!".green().bold());
    Ok(())
}

fn install(path: &Path) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if let Some((config_path, format)) = find_config_file(path) {
        let config = parse_config(&config_path, &format)?;
        let build = config.build.as_ref().ok_or("No build section")?;
        let mut target_path = path.join(&build.target);
        if !target_path.exists() {
            eprintln!("{}", "Target not built".red().bold());
            return Ok(());
        }
        let install_prefix = PathBuf::from("/usr/local");
        match build.build_type.as_str() {
            "executable" => {
                let bin_dir = install_prefix.join("bin");
                fs::create_dir_all(&bin_dir)?;
                fs::copy(&target_path, bin_dir.join(&config.metadata.name))?;
            }
            "shared" => {
                let lib_dir = install_prefix.join("lib");
                fs::create_dir_all(&lib_dir)?;
                target_path = target_path.with_extension("so");
                fs::copy(&target_path, lib_dir.join(target_path.file_name().unwrap()))?;
            }
            "static" => {
                let lib_dir = install_prefix.join("lib");
                fs::create_dir_all(&lib_dir)?;
                target_path = target_path.with_extension("a");
                fs::copy(&target_path, lib_dir.join(target_path.file_name().unwrap()))?;
            }
            _ => {}
        }
        // Config files to /etc/<project>
        if let Some((config_file, _)) = find_config_file(path) {
            let etc_dir = PathBuf::from("/etc").join(&config.metadata.name);
            fs::create_dir_all(&etc_dir)?;
            fs::copy(config_file, etc_dir.join("config"))?;
        }
        println!("{}", "Installation complete!".green().bold());
    } else {
        eprintln!("{}", "No config file found".red().bold());
    }
    Ok(())
}
