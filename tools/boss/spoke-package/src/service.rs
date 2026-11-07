//! The launchd and systemd definitions, rendered from one description.
//!
//! The environment the daemon runs in is defined once, in [`Spec::environment`],
//! and used twice: it is written into the service definition, and the installer
//! passes the same variables when it runs checks as the service user, so the
//! checks see what the service will see.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail, ensure};

pub use boss_spoke_preflight::HOMES_ENV;

use crate::manifest::is_token;

pub const SYSTEMD_UNIT: &str = "boss-spoke.service";
pub const LAUNCHD_LABEL: &str = "dev.boss.spoke";

const SYSTEMD_TEMPLATE: &str = include_str!("../service/boss-spoke.service.in");
const LAUNCHD_TEMPLATE: &str = include_str!("../service/dev.boss.spoke.plist.in");

/// Everything a service definition needs.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct Spec {
    pub user: String,
    pub home: PathBuf,
    pub state_dir: PathBuf,
    pub binary: PathBuf,
    pub host_id: String,
    pub hub_url: String,
    /// The service's `PATH`: where it finds tmux, cube, jj, git, gh, python3
    /// and the driver CLIs.
    pub path: String,
}

impl Spec {
    pub fn credentials_dir(&self) -> PathBuf {
        self.state_dir.join("credentials")
    }

    pub fn homes_dir(&self) -> PathBuf {
        self.state_dir.join("homes")
    }

    pub fn environment(&self) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), self.home.display().to_string()),
            ("PATH".into(), self.path.clone()),
            (HOMES_ENV.into(), self.homes_dir().display().to_string()),
        ]
    }

    pub fn arguments(&self) -> Vec<String> {
        vec![
            self.binary.display().to_string(),
            "--state-dir".into(),
            self.state_dir.display().to_string(),
            "--host-id".into(),
            self.host_id.clone(),
            "--hub".into(),
            self.hub_url.clone(),
            "--tls-dir".into(),
            self.credentials_dir().display().to_string(),
        ]
    }

    /// Every value ends up inside a unit file or a plist, so only plain tokens
    /// and paths are accepted: no whitespace, quoting, `%` specifiers or markup.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.user
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
                && self.user.len() <= 32
                && self
                    .user
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-')),
            "service user {:?} is not a plain account name",
            self.user
        );
        ensure!(
            is_token(&self.host_id, 128),
            "host id {:?} is not a plain token",
            self.host_id
        );
        let authority = self.hub_url.strip_prefix("https://").unwrap_or_default();
        ensure!(
            !authority.is_empty()
                && authority
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '/')),
            "hub URL {:?} must be https://host[:port]",
            self.hub_url
        );
        for path in [&self.home, &self.state_dir, &self.binary] {
            plain_absolute(path)?;
        }
        ensure!(
            self.path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':')),
            "PATH {:?} contains characters that need quoting",
            self.path
        );
        Ok(())
    }

    pub fn render_systemd(&self) -> String {
        let environment: String = self
            .environment()
            .iter()
            .map(|(key, value)| format!("Environment={key}={value}\n"))
            .collect();
        fill(
            SYSTEMD_TEMPLATE,
            &[
                ("credentials_dir", self.credentials_dir().display().to_string()),
                ("user", self.user.clone()),
                ("group", self.user.clone()),
                ("state_dir", self.state_dir.display().to_string()),
                ("environment", environment.trim_end().to_owned()),
                ("exec_start", self.arguments().join(" ")),
            ],
        )
    }

    pub fn render_launchd(&self) -> String {
        let arguments: Vec<String> = self
            .arguments()
            .iter()
            .map(|argument| format!("\t\t<string>{}</string>", xml(argument)))
            .collect();
        let environment: Vec<String> = self
            .environment()
            .iter()
            .map(|(key, value)| format!("\t\t<key>{}</key>\n\t\t<string>{}</string>", xml(key), xml(value)))
            .collect();
        fill(
            LAUNCHD_TEMPLATE,
            &[
                ("label", LAUNCHD_LABEL.to_owned()),
                ("user", xml(&self.user)),
                ("group", xml(&self.user)),
                ("arguments", arguments.join("\n")),
                ("environment", environment.join("\n")),
                ("state_dir", xml(&self.state_dir.display().to_string())),
                ("credentials_dir", xml(&self.credentials_dir().display().to_string())),
            ],
        )
    }
}

/// An absolute path of plain components made of `[A-Za-z0-9._-]`.
pub fn plain_absolute(path: &Path) -> Result<()> {
    ensure!(path.is_absolute(), "{} must be an absolute path", path.display());
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) if name.to_str().is_some_and(|name| is_token(name, 255)) => {}
            _ => bail!("{} has a component that is not a plain name", path.display()),
        }
    }
    Ok(())
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn fill(template: &str, values: &[(&str, String)]) -> String {
    let mut text = template.to_owned();
    for (key, value) in values {
        text = text.replace(&format!("{{{{{key}}}}}"), value);
    }
    assert!(!text.contains("{{"), "service template has an unfilled placeholder");
    text
}
