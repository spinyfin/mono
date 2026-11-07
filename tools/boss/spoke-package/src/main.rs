//! `boss-spoke-package`: build, sign, verify and install signed spoke packages,
//! and check a host's readiness as its service user.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use boss_spoke_package::install::{self, Host, InstallRecord, Layout, Request};
use boss_spoke_package::manifest::{self, Platform, SigningKey, TrustedKey};
use boss_spoke_package::system::{Staged, System, run_as};
use boss_spoke_package::{build, read_owner_only};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Build, sign, verify and install signed Boss spoke packages")]
enum Cli {
    /// Generate the release owner's Ed25519 key pair.
    Keygen {
        #[arg(long)]
        private_out: PathBuf,
        #[arg(long)]
        public_out: PathBuf,
    },
    /// Package the spoke daemon and this installer, and write the unsigned manifest.
    Build {
        #[arg(long)]
        spoke_bin: PathBuf,
        #[arg(long)]
        release_id: String,
        /// Defaults to the platform this tool is running on.
        #[arg(long)]
        platform: Option<String>,
        /// Where the package will be published, as an https URL prefix.
        #[arg(long)]
        url_base: String,
        #[arg(long)]
        out_dir: PathBuf,
    },
    /// Sign a manifest with the release owner's key (an owner-only file).
    Sign {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        key: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Check a release without installing it.
    Verify {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        signature: PathBuf,
        #[arg(long)]
        package: PathBuf,
        #[arg(long)]
        trusted_key: PathBuf,
    },
    /// Verify a release and install it, as root. Enrolls nothing.
    Install(InstallArgs),
    /// Check this host as the service user, in the installed service environment.
    Preflight {
        #[arg(long, default_value = "/etc/boss-spoke/install.json")]
        config: PathBuf,
        /// Only the storage checks.
        #[arg(long)]
        storage_only: bool,
        /// Repositories (URLs) the host must be able to read.
        #[arg(long)]
        repo: Vec<String>,
        /// Run the checks in this process instead of as the service user.
        #[arg(long, hide = true)]
        inline: bool,
        #[arg(long, hide = true)]
        state_dir: Option<PathBuf>,
        #[arg(long, hide = true)]
        hub_url: Option<String>,
    },
}

#[derive(clap::Args)]
struct InstallArgs {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    signature: PathBuf,
    #[arg(long)]
    package: PathBuf,
    /// The release owner's public key. Required the first time; afterwards the
    /// key this host pinned is used.
    #[arg(long)]
    trusted_key: Option<PathBuf>,
    #[arg(long)]
    host_id: String,
    #[arg(long)]
    hub_url: String,
    /// The hub's CA certificate (PEM).
    #[arg(long)]
    hub_ca: PathBuf,
    #[arg(long, default_value = install::DEFAULT_USER)]
    user: String,
    /// Stage everything under this directory instead of `/`, without creating
    /// the account or touching systemd or launchd (no root needed). For
    /// inspecting a release.
    #[arg(long)]
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli {
        Cli::Keygen {
            private_out,
            public_out,
        } => {
            let (seed, public) = SigningKey::generate()?;
            let mut private = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&private_out)
                .with_context(|| format!("{} must not already exist", private_out.display()))?;
            private.write_all(format!("{seed}\n").as_bytes())?;
            fs::write(&public_out, public.to_text())?;
            println!("public key fingerprint: {}", public.fingerprint());
        }
        Cli::Build {
            spoke_bin,
            release_id,
            platform,
            url_base,
            out_dir,
        } => {
            let platform = match platform {
                Some(name) => Platform::parse(&name)?,
                None => Platform::host()?,
            };
            let installer = fs::read(std::env::current_exe()?)?;
            let built = build(fs::read(&spoke_bin)?, installer, &release_id, platform, &url_base)?;
            fs::create_dir_all(&out_dir)?;
            fs::write(out_dir.join(&built.file_name), &built.package)?;
            let manifest = out_dir.join(manifest_name(platform));
            fs::write(&manifest, built.manifest.to_bytes())?;
            println!("{}\n{}", out_dir.join(&built.file_name).display(), manifest.display());
        }
        Cli::Sign { manifest, key, out } => {
            let key = SigningKey::from_text(&read_owner_only(&key)?)?;
            fs::write(out, key.sign(&fs::read(manifest)?))?;
            // The release log records which key signed, never the key.
            println!("signed with key {}", key.public().fingerprint());
        }
        Cli::Verify {
            manifest,
            signature,
            package,
            trusted_key,
        } => {
            let key = read_key(&trusted_key)?;
            let verified = manifest::verify_release(
                &key,
                &fs::read(manifest)?,
                &fs::read_to_string(signature)?,
                &fs::read(package)?,
                Platform::host()?,
            )?;
            println!(
                "verified release {} for {}",
                verified.manifest.release_id,
                verified.platform.as_str()
            );
        }
        Cli::Install(args) => return install_command(args),
        Cli::Preflight {
            config,
            storage_only,
            repo,
            inline,
            state_dir,
            hub_url,
        } => {
            return if inline {
                inline_preflight(
                    state_dir.context("--inline needs --state-dir")?,
                    storage_only,
                    hub_url,
                    repo,
                )
            } else {
                service_user_preflight(&config, storage_only, &repo)
            };
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn read_key(path: &Path) -> Result<TrustedKey> {
    Ok(TrustedKey::parse(
        &fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?,
    )?)
}

fn manifest_name(platform: Platform) -> String {
    format!("boss-spoke-{}.manifest.json", platform.as_str())
}

fn install_command(args: InstallArgs) -> Result<ExitCode> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    if args.root.is_none() && unsafe { libc::geteuid() } != 0 {
        bail!("install writes system directories and a service definition; run it as root");
    }
    let platform = Platform::host()?;
    let staged = args.root.is_some();
    let key = args.trusted_key.as_deref().map(read_key).transpose()?;
    let manifest = fs::read(args.manifest)?;
    let signature = fs::read_to_string(args.signature)?;
    let package = fs::read(args.package)?;
    let hub_ca = fs::read(args.hub_ca)?;
    let request = Request::builder()
        .manifest(&manifest)
        .signature(&signature)
        .package(&package)
        .maybe_trusted_key(key)
        .host_id(&args.host_id)
        .hub_url(&args.hub_url)
        .hub_ca(&hub_ca)
        .platform(platform)
        .layout(Layout {
            platform,
            user: args.user,
            root: args.root,
        })
        .build();
    let host: &dyn Host = if staged { &Staged } else { &System { platform } };
    let report = install::install(host, &request)?;
    println!("release {} for {}", report.release_id, platform.as_str());
    println!("release key fingerprint: {}", report.key_fingerprint);
    if report.changed.is_empty() {
        println!("nothing changed: this host already matches");
    } else {
        println!("changed: {}", report.changed.join(", "));
    }
    if report.restarted {
        println!("service restarted; running workers and the journal were kept");
    } else if !report.enrolled {
        println!(
            "service not started: this host is not enrolled. Enroll it as the service user, then start the service."
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// Re-runs this command as the service user with the installed service
/// environment, so the checks see what the service will see.
fn service_user_preflight(config: &Path, storage_only: bool, repos: &[String]) -> Result<ExitCode> {
    let record = InstallRecord::read(config)?;
    let layout = record.layout()?;
    let spec = layout.spec(&record.host_id, &record.hub_url);
    let account = System {
        platform: layout.platform,
    }
    .lookup_user(&layout.user)?
    .with_context(|| format!("service user {} does not exist", layout.user))?;
    let mut args = vec![
        "preflight".to_owned(),
        "--inline".into(),
        "--state-dir".into(),
        layout.state_dir().display().to_string(),
        "--hub-url".into(),
        record.hub_url.clone(),
    ];
    if storage_only {
        args.push("--storage-only".into());
    }
    for repo in repos {
        args.extend(["--repo".to_owned(), repo.clone()]);
    }
    let output = run_as(&account, &spec.environment(), &std::env::current_exe()?, &args)
        .context("run preflight as root or as the service user")?;
    print!("{}", output.output);
    Ok(if output.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn inline_preflight(
    state_dir: PathBuf,
    storage_only: bool,
    hub_url: Option<String>,
    repos: Vec<String>,
) -> Result<ExitCode> {
    let config = boss_spoke_preflight::Config::builder()
        .state_dir(state_dir)
        .maybe_homes_root(std::env::var_os(boss_spoke_preflight::HOMES_ENV).map(PathBuf::from))
        .maybe_hub(hub_url.as_deref().and_then(boss_spoke_preflight::hub_authority))
        .repos(repos)
        .build();
    let outcomes = if storage_only {
        boss_spoke_preflight::run_storage(&config)
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(boss_spoke_preflight::run(&config))
    };
    for outcome in &outcomes {
        match &outcome.result {
            Ok(detail) => println!("PASS {}: {detail}", outcome.name),
            Err(reason) => println!("FAIL {}: {reason}", outcome.name),
        }
    }
    let failed = boss_spoke_preflight::failed(&outcomes);
    if failed.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }
    println!("failed_preflights: {}", failed.join(", "));
    Ok(ExitCode::FAILURE)
}
