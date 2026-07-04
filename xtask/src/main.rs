use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIGNING_IDENTITY: &str = "Developer ID Application";
const SPARKLE_VERSION: &str = "2.9.0";
const SPARKLE_RELEASE_URL: &str =
    "https://github.com/sparkle-project/Sparkle/releases/download/2.9.0/Sparkle-for-Swift-Package-Manager.zip";
const SPARKLE_KEY_ACCOUNT: &str = "CharlonTank-agents-sleep-preventer";
const LEGACY_SPARKLE_KEY_ACCOUNT: &str = "CharlonTank-claude-sleep-preventer";
const SPARKLE_APPCAST_ASSET_NAME: &str = "appcast.xml";
const GITHUB_REPO: &str = "CharlonTank/agents-sleep-preventer";
const OWNED_HOOK_MARKERS: [&str; 4] = [
    "AgentsSleepPreventer.app/Contents/MacOS/asp",
    "/usr/local/bin/asp",
    "/usr/local/bin/agents-sleep-preventer",
    "claude-sleep-preventer",
];

struct AppcastArtifacts {
    appcast_path: PathBuf,
    release_notes_path: PathBuf,
}

#[derive(Parser)]
#[command(name = "xtask")]
#[command(about = "Build tasks for Agents Sleep Preventer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a signed and notarized DMG
    BuildDmg {
        /// Skip notarization (for local testing)
        #[arg(long)]
        skip_notarize: bool,
    },
    /// Clean all ASP data from the system
    Clean {
        /// Keep Whisper model data (~500 MB)
        #[arg(long)]
        keep_model: bool,
    },
    /// Complete test: clean, build DMG, and open it
    #[command(name = "complete-test", alias = "test")]
    CompleteTest {
        /// Skip notarization (for faster testing)
        #[arg(long)]
        skip_notarize: bool,
        /// Keep Whisper model data (~500 MB)
        #[arg(long)]
        keep_model: bool,
    },
    /// Replace /Applications app with the latest build
    ReplaceApp {
        /// Open app after replacing
        #[arg(long)]
        open: bool,
    },
    /// Bump version and build/publish release artifacts
    Release {
        /// New version to release (e.g. 2.5.1)
        version: String,
        /// Skip notarization (for dry runs)
        #[arg(long)]
        skip_notarize: bool,
        /// Upload DMG to GitHub release
        #[arg(long)]
        upload: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Ensure we're in the right directory
    let project_root = project_root()?;
    std::env::set_current_dir(&project_root)?;

    match cli.command {
        Commands::BuildDmg { skip_notarize } => build_dmg(skip_notarize),
        Commands::Clean { keep_model } => clean(keep_model),
        Commands::CompleteTest {
            skip_notarize,
            keep_model,
        } => complete_test(skip_notarize, keep_model),
        Commands::ReplaceApp { open } => replace_app(open),
        Commands::Release {
            version,
            skip_notarize,
            upload,
        } => release(&version, skip_notarize, upload),
    }
}

fn complete_test(skip_notarize: bool, keep_model: bool) -> Result<()> {
    println!("=== Complete Test ===\n");

    // Step 1: Clean system before installing/testing
    println!(">>> Step 1: Cleaning system...\n");
    clean(keep_model)?;

    // Step 2: Build DMG
    println!("\n>>> Step 2: Building DMG...\n");
    build_dmg(skip_notarize)?;

    // Step 3: Open DMG
    println!("\n>>> Step 3: Opening DMG...");
    let version = get_version()?;
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    run("open", &[&dmg_name])?;

    println!("\n=== Test ready! ===");
    println!("DMG is open. Drag the app to Applications and launch it.");

    Ok(())
}

fn project_root() -> Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for dir in manifest_dir.ancestors() {
        let cargo_toml_path = dir.join("Cargo.toml");
        let Ok(cargo_toml) = fs::read_to_string(&cargo_toml_path) else {
            continue;
        };
        if cargo_toml.contains("name = \"agents-sleep-preventer\"") {
            return Ok(dir.to_path_buf());
        }
    }

    bail!(
        "Could not locate agents-sleep-preventer Cargo.toml from {}",
        manifest_dir.display()
    )
}

fn get_version() -> Result<String> {
    let cargo_toml = fs::read_to_string("Cargo.toml")?;
    let parsed: toml::Value = cargo_toml.parse()?;
    let version = parsed["package"]["version"]
        .as_str()
        .context("Could not find version in Cargo.toml")?;
    Ok(version.to_string())
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    println!("  Running: {} {}", cmd, args.join(" "));
    let status = Command::new(cmd).args(args).status()?;
    if !status.success() {
        bail!("Command failed: {} {:?}", cmd, args);
    }
    Ok(())
}

fn run_in_dir(cmd: &str, args: &[&str], dir: &Path) -> Result<()> {
    println!(
        "  Running: {} {} (cwd={})",
        cmd,
        args.join(" "),
        dir.display()
    );
    let status = Command::new(cmd).current_dir(dir).args(args).status()?;
    if !status.success() {
        bail!("Command failed: {} {:?} (cwd={})", cmd, args, dir.display());
    }
    Ok(())
}

fn run_output(cmd: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(cmd).args(args).output()?;
    if !output.status.success() {
        bail!(
            "Command failed: {} {:?}\n{}",
            cmd,
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn ensure_sparkle() -> Result<PathBuf> {
    let sparkle_root = Path::new("target").join("sparkle").join(SPARKLE_VERSION);
    let framework_dir = sparkle_root
        .join("Sparkle.xcframework")
        .join("macos-arm64_x86_64")
        .join("Sparkle.framework");
    let generate_appcast = sparkle_root.join("bin").join("generate_appcast");
    let generate_keys = sparkle_root.join("bin").join("generate_keys");

    if framework_dir.exists() && generate_appcast.exists() && generate_keys.exists() {
        return Ok(sparkle_root);
    }

    println!("  Downloading Sparkle {}...", SPARKLE_VERSION);
    fs::create_dir_all("target/sparkle")?;

    let archive_path = Path::new("target")
        .join("sparkle")
        .join(format!("Sparkle-{}.zip", SPARKLE_VERSION));
    if archive_path.exists() {
        fs::remove_file(&archive_path)?;
    }
    if sparkle_root.exists() {
        fs::remove_dir_all(&sparkle_root)?;
    }

    run(
        "curl",
        &[
            "-L",
            "-o",
            archive_path.to_str().unwrap(),
            SPARKLE_RELEASE_URL,
        ],
    )?;

    fs::create_dir_all(&sparkle_root)?;
    run(
        "ditto",
        &[
            "-x",
            "-k",
            archive_path.to_str().unwrap(),
            sparkle_root.to_str().unwrap(),
        ],
    )?;

    if !framework_dir.exists() {
        bail!(
            "Sparkle framework missing after extraction: {}",
            framework_dir.display()
        );
    }

    Ok(sparkle_root)
}

fn github_repo_url() -> String {
    format!("https://github.com/{}", GITHUB_REPO)
}

fn latest_appcast_url() -> String {
    format!(
        "{}/releases/latest/download/{}",
        github_repo_url(),
        SPARKLE_APPCAST_ASSET_NAME
    )
}

fn release_asset_base_url(version: &str) -> String {
    format!("{}/releases/download/v{}/", github_repo_url(), version)
}

fn release_tag_url(version: &str) -> String {
    format!("{}/releases/tag/v{}", github_repo_url(), version)
}

fn release_title(version: &str) -> String {
    format!("Agents Sleep Preventer {}", version)
}

fn copy_with_ditto(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        if dst.is_dir() {
            fs::remove_dir_all(dst)?;
        } else {
            fs::remove_file(dst)?;
        }
    }

    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    run("ditto", &[src.to_str().unwrap(), dst.to_str().unwrap()])?;
    Ok(())
}

fn codesign_runtime(path: &Path) -> Result<()> {
    run(
        "codesign",
        &[
            "--force",
            "--options",
            "runtime",
            "--sign",
            SIGNING_IDENTITY,
            path.to_str().unwrap(),
        ],
    )
}

fn codesign_runtime_with_entitlements(path: &Path, entitlements: &Path) -> Result<()> {
    run(
        "codesign",
        &[
            "--force",
            "--options",
            "runtime",
            "--entitlements",
            entitlements.to_str().unwrap(),
            "--sign",
            SIGNING_IDENTITY,
            path.to_str().unwrap(),
        ],
    )
}

fn codesign_dmg(path: &Path) -> Result<()> {
    run(
        "codesign",
        &[
            "--force",
            "--sign",
            SIGNING_IDENTITY,
            path.to_str().unwrap(),
        ],
    )?;
    run(
        "codesign",
        &["--verify", "--verbose=2", path.to_str().unwrap()],
    )
}

fn ensure_whisper_cli() -> Result<()> {
    let whisper_cli_path = Path::new("/tmp/whisper.cpp/build/bin/whisper-cli");
    if whisper_cli_path.exists() {
        return Ok(());
    }

    println!("  whisper-cli missing, building from source...");

    let repo_dir = Path::new("/tmp/whisper.cpp");
    if !repo_dir.exists() {
        run(
            "git",
            &[
                "clone",
                "https://github.com/ggerganov/whisper.cpp",
                "/tmp/whisper.cpp",
            ],
        )?;
    }

    let build_dir = repo_dir.join("build");
    fs::create_dir_all(&build_dir)?;

    let is_arm = matches!(std::env::consts::ARCH, "aarch64" | "arm");
    let mut cmake_args = vec![
        "..".to_string(),
        "-DBUILD_SHARED_LIBS=OFF".to_string(),
        "-DGGML_METAL=ON".to_string(),
        "-DCMAKE_BUILD_TYPE=Release".to_string(),
        "-DGGML_CCACHE=OFF".to_string(),
        "-DGGML_OPENMP=OFF".to_string(),
        "-DCMAKE_WARN_DEPRECATED=OFF".to_string(),
    ];
    if is_arm {
        cmake_args.push("-DARM_NATIVE_FLAG=-mcpu=native".to_string());
    }
    let cmake_args_ref: Vec<&str> = cmake_args.iter().map(String::as_str).collect();
    run_in_dir("cmake", &cmake_args_ref, &build_dir)?;

    let jobs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8);
    run_in_dir("make", &[&format!("-j{}", jobs), "whisper-cli"], &build_dir)?;

    if !whisper_cli_path.exists() {
        bail!("whisper-cli build failed (missing /tmp/whisper.cpp/build/bin/whisper-cli)");
    }

    Ok(())
}

fn build_release_notes(version: &str, output_path: &Path) -> Result<()> {
    let overrides_path = Path::new("release-notes").join(format!("{}.md", version));
    if overrides_path.exists() {
        fs::copy(overrides_path, output_path)?;
        return Ok(());
    }

    let notes = format!(
        "# Agents Sleep Preventer {version}\n\nThis release includes improvements and fixes.\n\nSee the full release notes on GitHub:\n{release_url}\n",
        release_url = release_tag_url(version)
    );
    fs::write(output_path, notes)?;
    Ok(())
}

fn generate_appcast(version: &str) -> Result<AppcastArtifacts> {
    let sparkle_root = ensure_sparkle()?;
    let generate_appcast_bin = sparkle_root.join("bin").join("generate_appcast");
    let staging_dir = Path::new("target").join("release").join("sparkle-appcast");
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir)?;
    }
    fs::create_dir_all(&staging_dir)?;

    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    let dmg_source = Path::new(&dmg_name);
    let dmg_staging_path = staging_dir.join(&dmg_name);
    fs::copy(dmg_source, &dmg_staging_path)?;

    let release_notes_path = staging_dir.join(format!("AgentsSleepPreventer-{}.md", version));
    build_release_notes(version, &release_notes_path)?;

    let appcast_path = staging_dir.join(SPARKLE_APPCAST_ASSET_NAME);
    let download_prefix = release_asset_base_url(version);
    let release_url = release_tag_url(version);
    let repo_url = github_repo_url();
    let mut generated = false;
    for account in [SPARKLE_KEY_ACCOUNT, LEGACY_SPARKLE_KEY_ACCOUNT] {
        let args = vec![
            "--account",
            account,
            "--download-url-prefix",
            download_prefix.as_str(),
            "--embed-release-notes",
            "--full-release-notes-url",
            release_url.as_str(),
            "--link",
            repo_url.as_str(),
            "--versions",
            version,
            "-o",
            appcast_path.to_str().unwrap(),
            staging_dir.to_str().unwrap(),
        ];

        println!("  Generating Sparkle appcast with key account: {}", account);
        if Command::new(&generate_appcast_bin)
            .args(&args)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            generated = true;
            break;
        }

        if account == SPARKLE_KEY_ACCOUNT {
            println!(
                "  Sparkle key account {} failed, trying legacy account {}...",
                SPARKLE_KEY_ACCOUNT, LEGACY_SPARKLE_KEY_ACCOUNT
            );
        }
    }

    if !generated {
        bail!(
            "Failed to generate Sparkle appcast with key accounts {} or {}",
            SPARKLE_KEY_ACCOUNT,
            LEGACY_SPARKLE_KEY_ACCOUNT
        );
    }

    if !appcast_path.exists() {
        bail!(
            "Failed to generate Sparkle appcast at {}",
            appcast_path.display()
        );
    }

    verify_appcast_file(version, &appcast_path)?;

    Ok(AppcastArtifacts {
        appcast_path,
        release_notes_path,
    })
}

fn build_dmg(skip_notarize: bool) -> Result<()> {
    let version = get_version()?;
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    let sparkle_root = ensure_sparkle()?;
    let sparkle_framework_slice = sparkle_root.join("Sparkle.xcframework/macos-arm64_x86_64");
    let sparkle_framework = sparkle_framework_slice.join("Sparkle.framework");

    println!("=== Building Agents Sleep Preventer v{} DMG ===\n", version);

    // Step 1: Build release
    println!("[1/9] Building release...");
    run("cargo", &["build", "--release"])?;

    // Step 2: Build Swift menubar app
    println!("[2/9] Building menubar app (Swift)...");
    let menubar_src = Path::new("swift/menubar.swift");
    if !menubar_src.exists() {
        bail!("swift/menubar.swift not found");
    }
    fs::create_dir_all("target/release")?;
    run(
        "swiftc",
        &[
            "swift/menubar.swift",
            "-parse-as-library",
            "-O",
            "-F",
            sparkle_framework_slice.to_str().unwrap(),
            "-framework",
            "Sparkle",
            "-Xlinker",
            "-rpath",
            "-Xlinker",
            "@executable_path/../Frameworks",
            "-o",
            "target/release/AgentsSleepPreventer",
        ],
    )?;

    // Step 3: Ensure whisper-cli
    println!("[3/9] Ensuring whisper-cli...");
    ensure_whisper_cli()?;
    let whisper_cli_path = Path::new("/tmp/whisper.cpp/build/bin/whisper-cli");

    // Step 4: Create app bundle
    println!("[4/9] Creating app bundle...");
    let bundle_dir = Path::new("target/release/bundle");
    let app_dir = bundle_dir.join("AgentsSleepPreventer.app");
    let contents_dir = app_dir.join("Contents");
    let macos_dir = contents_dir.join("MacOS");
    let frameworks_dir = contents_dir.join("Frameworks");
    let resources_dir = contents_dir.join("Resources");

    // Clean and recreate
    if bundle_dir.exists() {
        fs::remove_dir_all(bundle_dir)?;
    }
    fs::create_dir_all(&macos_dir)?;
    fs::create_dir_all(&frameworks_dir)?;
    fs::create_dir_all(&resources_dir)?;

    // Copy main app binary (Swift)
    fs::copy(
        "target/release/AgentsSleepPreventer",
        macos_dir.join("AgentsSleepPreventer"),
    )?;
    // Copy Rust CLI binary
    fs::copy("target/release/asp", macos_dir.join("asp"))?;
    fs::copy("Info.plist", contents_dir.join("Info.plist"))?;
    fs::copy("AppIcon.icns", resources_dir.join("AppIcon.icns"))?;
    copy_with_ditto(
        &sparkle_framework,
        &frameworks_dir.join("Sparkle.framework"),
    )?;

    // Copy bundled binaries to Resources
    fs::copy(whisper_cli_path, resources_dir.join("whisper-cli"))?;

    // Step 5: Sign bundled binaries and Sparkle before the app bundle itself
    println!("[5/9] Signing bundled binaries...");
    codesign_runtime(&macos_dir.join("asp"))?;
    codesign_runtime(&resources_dir.join("whisper-cli"))?;
    let sparkle_bundle = frameworks_dir.join("Sparkle.framework");
    codesign_runtime(&sparkle_bundle.join("Versions/B/Autoupdate"))?;
    codesign_runtime(&sparkle_bundle.join("Versions/B/XPCServices/Downloader.xpc"))?;
    codesign_runtime(&sparkle_bundle.join("Versions/B/XPCServices/Installer.xpc"))?;
    codesign_runtime(&sparkle_bundle.join("Versions/B/Updater.app"))?;
    codesign_runtime(&sparkle_bundle)?;

    // Step 7: Sign the app with entitlements
    println!("[6/9] Signing app with entitlements...");
    codesign_runtime_with_entitlements(&app_dir, Path::new("Entitlements.plist"))?;

    // Step 8: Create DMG staging folder with Applications symlink
    println!("[7/9] Creating DMG staging folder...");
    let staging_dir = Path::new("target/release/dmg-staging");
    if staging_dir.exists() {
        fs::remove_dir_all(staging_dir)?;
    }
    fs::create_dir_all(staging_dir)?;

    // Copy app to staging preserving Sparkle framework symlinks and code signatures
    copy_with_ditto(&app_dir, &staging_dir.join("AgentsSleepPreventer.app"))?;

    // Create Applications symlink - THIS IS THE KEY PART
    #[cfg(unix)]
    std::os::unix::fs::symlink("/Applications", staging_dir.join("Applications"))?;

    // Step 9: Create DMG
    println!("[8/9] Creating DMG...");
    if Path::new(&dmg_name).exists() {
        fs::remove_file(&dmg_name)?;
    }
    run(
        "hdiutil",
        &[
            "create",
            "-volname",
            "Agents Sleep Preventer",
            "-srcfolder",
            staging_dir.to_str().unwrap(),
            "-ov",
            "-format",
            "UDZO",
            &dmg_name,
        ],
    )?;

    println!("  Signing DMG...");
    codesign_dmg(Path::new(&dmg_name))?;

    // Cleanup staging
    fs::remove_dir_all(staging_dir)?;

    if skip_notarize {
        println!("\n[SKIP] Skipping notarization (--skip-notarize flag set)");
    } else {
        // Step 10: Notarize & Staple
        println!("[10/10] Notarizing (this may take a few minutes)...");
        run(
            "xcrun",
            &[
                "notarytool",
                "submit",
                &dmg_name,
                "--keychain-profile",
                "notary",
                "--wait",
            ],
        )?;

        println!("  Stapling...");
        run("xcrun", &["stapler", "staple", &dmg_name])?;
        run("stapler", &["validate", &dmg_name])?;
        run("spctl", &["-a", "-vv", "-t", "install", &dmg_name])?;
    }

    println!("\n=== Done! ===");
    println!("DMG created: {}", dmg_name);
    println!("\nTo install: open {}", dmg_name);

    Ok(())
}

fn hook_value_contains_owned_command(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => OWNED_HOOK_MARKERS
            .iter()
            .any(|marker| text.contains(marker)),
        serde_json::Value::Array(values) => values.iter().any(hook_value_contains_owned_command),
        serde_json::Value::Object(map) => map.values().any(hook_value_contains_owned_command),
        _ => false,
    }
}

fn remove_owned_hooks_from_group(group: &mut serde_json::Value) -> bool {
    let Some(hooks) = group
        .get_mut("hooks")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return false;
    };

    let before = hooks.len();
    hooks.retain(|hook| !hook_value_contains_owned_command(hook));
    before != hooks.len()
}

fn remove_owned_hook_groups(hooks: &mut serde_json::Value) -> bool {
    let Some(events) = hooks.as_object_mut() else {
        return false;
    };

    let mut changed = false;
    for groups in events.values_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };

        for group in groups.iter_mut() {
            if remove_owned_hooks_from_group(group) {
                changed = true;
            }
        }

        let before = groups.len();
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .map(|hooks| !hooks.is_empty())
                .unwrap_or(true)
        });
        changed |= before != groups.len();
    }

    changed
}

fn prune_empty_hook_events(hooks: &mut serde_json::Value) {
    if let Some(events) = hooks.as_object_mut() {
        events.retain(|_, groups| {
            groups
                .as_array()
                .map(|groups| !groups.is_empty())
                .unwrap_or(true)
        });
    }
}

fn clean_codex_hooks(home: &Path) -> Result<()> {
    let hooks_file = home.join(".codex/hooks.json");
    if !hooks_file.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&hooks_file)
        .with_context(|| format!("Failed to read {}", hooks_file.display()))?;
    let Ok(mut hooks_json) = serde_json::from_str::<serde_json::Value>(&content) else {
        println!(
            "  Could not parse {}, leaving it unchanged",
            hooks_file.display()
        );
        return Ok(());
    };

    let changed = hooks_json
        .get_mut("hooks")
        .map(remove_owned_hook_groups)
        .unwrap_or(false);
    if !changed {
        return Ok(());
    }

    if let Some(hooks) = hooks_json.get_mut("hooks") {
        prune_empty_hook_events(hooks);
    }

    if let Some(root) = hooks_json.as_object_mut() {
        let hooks_empty = root
            .get("hooks")
            .and_then(serde_json::Value::as_object)
            .map(|hooks| hooks.is_empty())
            .unwrap_or(false);
        if hooks_empty {
            root.remove("hooks");
        }
        if root.is_empty() {
            fs::remove_file(&hooks_file)
                .with_context(|| format!("Failed to remove {}", hooks_file.display()))?;
            println!("  Removed {}", hooks_file.display());
            return Ok(());
        }
    }

    fs::write(&hooks_file, serde_json::to_string_pretty(&hooks_json)?)
        .with_context(|| format!("Failed to write {}", hooks_file.display()))?;
    println!("  Removed ASP Codex hooks from {}", hooks_file.display());

    Ok(())
}

fn clean(keep_model: bool) -> Result<()> {
    println!("=== Agents Sleep Preventer Cleanup ===\n");
    if keep_model {
        println!("(Keeping Whisper models)\n");
    }

    // Kill running processes
    println!("Killing running processes...");
    let _ = Command::new("pkill")
        .args(["-f", "/AgentsSleepPreventer.app/Contents/MacOS/asp"])
        .status();
    let _ = Command::new("pkill").args(["-x", "asp"]).status();
    let _ = Command::new("pkill")
        .args(["-f", "claude-sleep-preventer"])
        .status();
    let _ = Command::new("pkill")
        .args(["-f", "AgentsSleepPreventer"])
        .status();
    let _ = Command::new("pkill")
        .args(["-f", "ClaudeSleepPreventer"])
        .status();

    // Remove app from Applications
    println!("Removing app...");
    let _ = fs::remove_dir_all("/Applications/AgentsSleepPreventer.app");
    let _ = fs::remove_dir_all("/Applications/ClaudeSleepPreventer.app");

    // Remove app data
    println!("Removing app data...");
    if let Some(home) = dirs::home_dir() {
        clean_app_support_dir(
            &home.join("Library/Application Support/AgentsSleepPreventer"),
            keep_model,
        )?;
        clean_app_support_dir(
            &home.join("Library/Application Support/ClaudeSleepPreventer"),
            keep_model,
        )?;
        clean_app_support_dir(&home.join(".local/share/AgentsSleepPreventer"), keep_model)?;
        clean_app_support_dir(&home.join(".local/share/ClaudeSleepPreventer"), keep_model)?;

        let _ = fs::remove_dir_all(home.join("Library/Logs/AgentsSleepPreventer"));
        let _ = fs::remove_dir_all(home.join("Library/Logs/ClaudeSleepPreventer"));
        let _ = fs::remove_dir_all(home.join("Library/Caches/AgentsSleepPreventer"));
        let _ = fs::remove_dir_all(home.join("Library/Caches/ClaudeSleepPreventer"));
        let _ = fs::remove_file(
            home.join("Library/Preferences/com.charlontank.agents-sleep-preventer.plist"),
        );
        let _ = fs::remove_file(
            home.join("Library/Preferences/com.charlontank.claude-sleep-preventer.plist"),
        );

        // Remove LaunchAgents
        println!("Removing LaunchAgents...");
        let uid = run_output("id", &["-u"])?;
        for label in [
            "com.charlontank.agents-sleep-preventer.plist",
            "com.charlontank.claude-sleep-preventer.plist",
        ] {
            let launch_agent = home.join("Library/LaunchAgents").join(label);
            if launch_agent.exists() {
                let _ = Command::new("launchctl")
                    .args([
                        "bootout",
                        &format!("gui/{}", uid.trim()),
                        launch_agent.to_str().unwrap(),
                    ])
                    .status();
                let _ = fs::remove_file(&launch_agent);
            }
        }

        // Remove Claude Code hooks (may be owned by root from old installations)
        println!("Removing Claude Code hooks...");
        let hooks_dir = home.join(".claude/hooks");
        if hooks_dir.exists() {
            // Try without sudo first
            if fs::remove_dir_all(&hooks_dir).is_err() {
                // If that fails (owned by root), use osascript with admin privileges
                let cmd = format!("rm -rf '{}'", hooks_dir.display());
                let applescript = format!(
                    "do shell script \"{}\" with administrator privileges",
                    cmd.replace("\"", "\\\"")
                );
                let _ = Command::new("osascript")
                    .args(["-e", &applescript])
                    .status();
            }
        }

        // Clean hooks from settings.json
        let settings_path = home.join(".claude/settings.json");
        if settings_path.exists() {
            println!("Cleaning settings.json...");
            if let Ok(content) = fs::read_to_string(&settings_path) {
                if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if json.get("hooks").is_some() {
                        json.as_object_mut().unwrap().remove("hooks");
                        let _ = fs::write(&settings_path, serde_json::to_string_pretty(&json)?);
                        println!("  Removed hooks from settings.json");
                    }
                }
            }
        }

        println!("Removing Codex hooks...");
        clean_codex_hooks(&home)?;
    }

    let _ = fs::remove_dir_all("/tmp/agents_working_pids");
    let _ = fs::remove_dir_all("/tmp/claude_working_pids");

    // Remove sudoers config
    println!("Removing sudoers config...");
    for sudoers_path in ["/etc/sudoers.d/agents-pmset", "/etc/sudoers.d/claude-pmset"] {
        let _ = Command::new("sudo")
            .args(["rm", "-f", sudoers_path])
            .status();
    }
    let _ = fs::remove_file("/usr/local/bin/asp");
    let _ = fs::remove_file("/usr/local/bin/agents-sleep-preventer");
    let _ = fs::remove_file("/usr/local/bin/claude-sleep-preventer");

    // Remove whisper-cli and models (Homebrew + local build)
    if keep_model {
        println!("Keeping whisper-cli and models...");
    } else {
        println!("Removing whisper-cli and models...");
        let _ = fs::remove_dir_all("/tmp/whisper.cpp");
        let _ = fs::remove_file("/opt/homebrew/bin/whisper-cli");
        let _ = fs::remove_file("/usr/local/bin/whisper-cli");
        let _ = fs::remove_dir_all("/opt/homebrew/share/whisper-cpp/models");
        let _ = fs::remove_dir_all("/usr/local/share/whisper-cpp/models");
    }

    // Reset TCC permissions
    println!("Resetting TCC permissions...");
    for permission in ["Microphone", "Accessibility", "ListenEvent"] {
        for bundle_id in [
            "com.charlontank.agents-sleep-preventer",
            "com.charlontank.claude-sleep-preventer",
        ] {
            let _ = Command::new("tccutil")
                .args(["reset", permission, bundle_id])
                .status();
        }
    }

    // Re-enable sleep
    println!("Re-enabling sleep...");
    let _ = Command::new("sudo")
        .args(["pmset", "-a", "disablesleep", "0"])
        .status();

    // Unmount any DMG
    println!("Unmounting DMG...");
    if Path::new("/Volumes/Agents Sleep Preventer").exists() {
        let _ = Command::new("hdiutil")
            .args(["detach", "/Volumes/Agents Sleep Preventer"])
            .status();
    }
    if Path::new("/Volumes/Claude Sleep Preventer").exists() {
        let _ = Command::new("hdiutil")
            .args(["detach", "/Volumes/Claude Sleep Preventer"])
            .status();
    }

    println!("\n=== Cleanup complete! ===");
    println!("You can now install a fresh version.");

    Ok(())
}

fn clean_app_support_dir(dir: &Path, keep_model: bool) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }

    if !keep_model {
        let _ = fs::remove_dir_all(dir);
        return Ok(());
    }

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new("models") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }

    Ok(())
}

fn replace_app(open_app: bool) -> Result<()> {
    let app_dir = Path::new("/Applications/AgentsSleepPreventer.app");
    if !app_dir.exists() {
        bail!("Missing /Applications/AgentsSleepPreventer.app");
    }
    let sparkle_root = ensure_sparkle()?;
    let sparkle_framework_slice = sparkle_root.join("Sparkle.xcframework/macos-arm64_x86_64");
    let sparkle_framework = sparkle_framework_slice.join("Sparkle.framework");

    println!("=== Replace App ===\n");
    println!("Building release...");
    run("cargo", &["build", "--release"])?;
    run(
        "swiftc",
        &[
            "swift/menubar.swift",
            "-parse-as-library",
            "-O",
            "-F",
            sparkle_framework_slice.to_str().unwrap(),
            "-framework",
            "Sparkle",
            "-Xlinker",
            "-rpath",
            "-Xlinker",
            "@executable_path/../Frameworks",
            "-o",
            "target/release/AgentsSleepPreventer",
        ],
    )?;
    let bin_path = app_dir.join("Contents/MacOS/asp");
    let menubar_path = app_dir.join("Contents/MacOS/AgentsSleepPreventer");
    let plist_path = app_dir.join("Contents/Info.plist");
    let resources_dir = app_dir.join("Contents/Resources");
    let frameworks_dir = app_dir.join("Contents/Frameworks");
    fs::copy("target/release/asp", &bin_path)?;
    fs::copy("target/release/AgentsSleepPreventer", &menubar_path)?;
    fs::create_dir_all(&resources_dir)?;
    fs::create_dir_all(&frameworks_dir)?;
    copy_with_ditto(
        &sparkle_framework,
        &frameworks_dir.join("Sparkle.framework"),
    )?;
    fs::copy("Info.plist", &plist_path)?;

    println!("Signing app...");
    run(
        "codesign",
        &[
            "--force",
            "--deep",
            "--sign",
            "-",
            app_dir.to_str().unwrap(),
        ],
    )?;

    if open_app {
        run("open", &[app_dir.to_str().unwrap()])?;
    }

    println!("App replaced: {}", app_dir.display());
    Ok(())
}

fn verify_appcast_file(version: &str, appcast_path: &Path) -> Result<()> {
    let content = fs::read_to_string(appcast_path)
        .with_context(|| format!("Failed to read {}", appcast_path.display()))?;
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    let required = [
        version.to_string(),
        dmg_name,
        release_asset_base_url(version),
        github_repo_url(),
        "sparkle:edSignature".to_string(),
    ];

    for needle in required {
        if !content.contains(&needle) {
            bail!(
                "Generated appcast {} does not contain required value: {}",
                appcast_path.display(),
                needle
            );
        }
    }

    Ok(())
}

fn release_exists(tag: &str) -> Result<bool> {
    let status = Command::new("gh")
        .args(["release", "view", tag, "--repo", GITHUB_REPO])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(status.success())
}

fn ensure_release_upload_ready(version: &str) -> Result<String> {
    let current = get_version()?;
    if current != version {
        bail!(
            "Version is still {}. Run `cargo xtask release {}` first, commit and push the version bump, then rerun `cargo xtask release {} --upload`.",
            current,
            version,
            version
        );
    }

    let tracked_status = run_output("git", &["status", "--porcelain", "--untracked-files=no"])?;
    if !tracked_status.trim().is_empty() {
        bail!(
            "Tracked files have uncommitted changes. Commit and push the release state before using --upload."
        );
    }

    let branch = run_output("git", &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        bail!("Could not determine current git branch for release upload");
    }

    run("git", &["fetch", "origin", branch])?;

    let head = run_output("git", &["rev-parse", "HEAD"])?;
    let remote_ref = format!("origin/{}", branch);
    let remote_head = run_output("git", &["rev-parse", &remote_ref])?;
    let head = head.trim().to_string();
    if head != remote_head.trim() {
        bail!(
            "Current HEAD is not pushed to origin/{}. Push the release commit before using --upload.",
            branch
        );
    }

    Ok(head)
}

fn ensure_github_release(version: &str, release_notes_path: &Path, target: &str) -> Result<()> {
    let tag = format!("v{}", version);
    let title = release_title(version);
    let notes = release_notes_path
        .to_str()
        .context("Invalid release notes path")?;

    if release_exists(&tag)? {
        run(
            "gh",
            &[
                "release",
                "edit",
                &tag,
                "--repo",
                GITHUB_REPO,
                "--title",
                &title,
                "--notes-file",
                notes,
                "--target",
                target,
                "--latest",
            ],
        )?;
    } else {
        run(
            "gh",
            &[
                "release",
                "create",
                &tag,
                "--repo",
                GITHUB_REPO,
                "--title",
                &title,
                "--notes-file",
                notes,
                "--target",
                target,
                "--latest",
            ],
        )?;
    }

    Ok(())
}

fn upload_release_assets(version: &str, appcast_path: &Path) -> Result<()> {
    let tag = format!("v{}", version);
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    run(
        "gh",
        &[
            "release",
            "upload",
            &tag,
            "--repo",
            GITHUB_REPO,
            &dmg_name,
            appcast_path.to_str().context("Invalid appcast path")?,
            "--clobber",
        ],
    )
}

fn verify_uploaded_release(version: &str) -> Result<()> {
    let tag = format!("v{}", version);
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    let release_json = run_output(
        "gh",
        &[
            "release",
            "view",
            &tag,
            "--repo",
            GITHUB_REPO,
            "--json",
            "assets,url",
        ],
    )?;
    let release: serde_json::Value = serde_json::from_str(&release_json)?;
    let assets = release
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .context("GitHub release response did not include assets")?;

    for required_asset in [&dmg_name, SPARKLE_APPCAST_ASSET_NAME] {
        let found = assets.iter().any(|asset| {
            asset
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(|name| name == required_asset)
                .unwrap_or(false)
        });
        if !found {
            bail!(
                "GitHub release {} is missing required asset {}",
                tag,
                required_asset
            );
        }
    }

    let latest_tag = run_output(
        "gh",
        &[
            "release",
            "view",
            "--repo",
            GITHUB_REPO,
            "--json",
            "tagName",
            "--jq",
            ".tagName",
        ],
    )?;
    if latest_tag.trim() != tag {
        bail!(
            "GitHub latest release is {}, expected {}",
            latest_tag.trim(),
            tag
        );
    }

    verify_latest_appcast(version)
}

fn verify_latest_appcast(version: &str) -> Result<()> {
    let dmg_name = format!("AgentsSleepPreventer-{}.dmg", version);
    let url = latest_appcast_url();

    for attempt in 1..=5 {
        let output = Command::new("curl").args(["-fsSL", &url]).output()?;
        if output.status.success() {
            let content = String::from_utf8_lossy(&output.stdout);
            if content.contains(version)
                && content.contains(&dmg_name)
                && content.contains("sparkle:edSignature")
            {
                return Ok(());
            }
        }

        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
    }

    bail!(
        "Latest Sparkle appcast at {} did not expose version {} with {}",
        url,
        version,
        dmg_name
    )
}

fn publish_release(version: &str, artifacts: &AppcastArtifacts, target: &str) -> Result<()> {
    ensure_github_release(version, &artifacts.release_notes_path, target)?;
    upload_release_assets(version, &artifacts.appcast_path)?;
    verify_uploaded_release(version)?;
    Ok(())
}

fn release(version: &str, skip_notarize: bool, upload: bool) -> Result<()> {
    println!("=== Release {} ===\n", version);

    let upload_target = if upload {
        Some(ensure_release_upload_ready(version)?)
    } else {
        None
    };

    bump_version(version)?;
    ensure_whisper_cli()?;
    build_dmg(skip_notarize)?;
    let appcast_artifacts = generate_appcast(version)?;

    if let Some(target) = upload_target {
        publish_release(version, &appcast_artifacts, &target)?;
    }

    println!("Release artifacts ready for {}", version);
    Ok(())
}

fn bump_version(version: &str) -> Result<()> {
    let current = get_version()?;

    if current == version {
        return Ok(());
    }

    replace_version_in_file("Cargo.toml", &current, version)?;
    replace_version_in_file("Cargo.lock", &current, version)?;
    replace_version_in_file("Info.plist", &current, version)?;
    replace_version_in_file("README.md", &current, version)?;
    replace_version_in_file("distribution.xml", &current, version)?;
    replace_version_in_file("distribution-synth.xml", &current, version)?;

    Ok(())
}

fn replace_version_in_file(path: &str, from: &str, to: &str) -> Result<()> {
    let content = fs::read_to_string(path)?;
    let updated = content.replace(from, to);
    if updated == content {
        bail!("No version string '{}' found in {}", from, path);
    }
    fs::write(path, updated)?;
    Ok(())
}
