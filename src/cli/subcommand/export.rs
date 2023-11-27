use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{stdout, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use crate::cli::CommandExecute;
use clap::Parser;

const LOCAL_STATE_DIR: &str = "/nix/var";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("The HOME environment variable is not set.")]
    HomeNotSet,

    #[error("__ETC_PROFILE_NIX_SOURCED is set, indicating the relevant environment variables have already been set.")]
    AlreadyRun,

    #[error("Some of the paths from Nix for XDG_DATA_DIR are not valid, due to an illegal character, like a colon.")]
    InvalidXdgDataDirs(Vec<PathBuf>),

    #[error("Some of the paths from Nix for PATH are not valid, due to an illegal character, like a colon.")]
    InvalidPathDirs(Vec<PathBuf>),

    #[error("Some of the paths from Nix for MANPATH are not valid, due to an illegal character, like a colon.")]
    InvalidManPathDirs(Vec<PathBuf>),
}

/**
Emit all the environment variables that should be set to use Nix.

Safety note: environment variables and values can contain any bytes except
for a null byte. This includes newlines and spaces, which requires careful
handling.

In `space-newline-separated` mode, `nix-installer` guarantees it will:

  * only emit keys that are alphanumeric with underscores,
  * only emit values without newlines

and will refuse to emit any output to stdout if the variables and values
would violate these safety rules.

In `null-separated` mode, `nix-installer` emits data in this format:

  KEYNAME\0VALUE\0KEYNAME\0VALUE\0

*/
#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Export {
    #[clap(long)]
    format: ExportFormat,

    #[clap(long)]
    sample_output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, clap::ValueEnum)]
enum ExportFormat {
    Fish,
    Sh,
}

#[async_trait::async_trait]
impl CommandExecute for Export {
    #[tracing::instrument(level = "trace", skip_all)]
    async fn execute(self) -> eyre::Result<ExitCode> {
        let env: HashMap<String, OsString> = match self.sample_output {
            Some(filename) => {
                // Note: not tokio File b/c I don't think serde_json has fancy async support?
                let file = std::fs::File::open(filename)?;
                let intermediate: HashMap<String, String> = serde_json::from_reader(file)?;
                intermediate
                    .into_iter()
                    .map(|(k, v)| (k, v.into()))
                    .collect()
            },
            None => {
                match calculate_environment() {
                    e @ Err(Error::AlreadyRun) => {
                        tracing::debug!("Ignored error: {:?}", e);
                        return Ok(ExitCode::SUCCESS);
                    },
                    Err(e) => {
                        tracing::warn!("Error setting up the environment for Nix: {:?}", e);
                        // Don't return an Err, because we don't want to suggest bug reports for predictable problems.
                        return Ok(ExitCode::FAILURE);
                    },
                    Ok(env) => env,
                }
            },
        };

        let mut export_env: HashMap<export::VariableName, OsString> = HashMap::new();
        for (k, v) in env.into_iter() {
            export_env.insert(k.try_into()?, v);
        }

        stdout().write_all(
            export::escape(
                match self.format {
                    ExportFormat::Fish => export::Encoding::Fish,
                    ExportFormat::Sh => export::Encoding::PosixShell,
                },
                export_env,
            )?
            .as_bytes(),
        )?;

        Ok(ExitCode::SUCCESS)
    }
}

fn nonempty_var_os(key: &str) -> Option<OsString> {
    env::var_os(key).filter(|val| !val.is_empty())
}

fn env_path(key: &str) -> Option<Vec<PathBuf>> {
    let path = env::var_os(key)?;

    if path.is_empty() {
        return Some(vec![]);
    }

    Some(env::split_paths(&path).collect())
}

pub fn calculate_environment() -> Result<HashMap<String, OsString>, Error> {
    let mut envs: HashMap<String, OsString> = HashMap::new();

    // Don't export variables twice.
    // @PORT-NOTE nix-profile-daemon.sh.in and nix-profile-daemon.fish.in implemented
    // this behavior, but it was not implemented in nix-profile.sh.in and nix-profile.fish.in
    // even though I believe it is desirable in both cases.
    if nonempty_var_os("__ETC_PROFILE_NIX_SOURCED") == Some("1".into()) {
        return Err(Error::AlreadyRun);
    }

    // @PORT-NOTE nix-profile.sh.in and nix-profile.fish.in check HOME and USER are set,
    // but not nix-profile-daemon.sh.in and nix-profile-daemon.fish.in.
    // The -daemon variants appear to just assume the values are set, which is probably
    // not safe, so we check it in all cases.
    let home = if let Some(home) = nonempty_var_os("HOME") {
        PathBuf::from(home)
    } else {
        return Err(Error::HomeNotSet);
    };

    envs.insert("__ETC_PROFILE_NIX_SOURCED".into(), "1".into());

    let nix_link: PathBuf = {
        let legacy_location = home.join(".nix-profile");
        let xdg_location = nonempty_var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"))
            .join("nix/profile");

        if xdg_location.is_symlink() {
            // In the future we'll prefer the legacy location, but
            // evidently this is the intended order preference:
            // https://github.com/NixOS/nix/commit/2b801d6e3c3a3be6feb6fa2d9a0b009fa9261b45
            xdg_location
        } else {
            legacy_location
        }
    };

    let nix_profiles = &[
        PathBuf::from(LOCAL_STATE_DIR).join("nix/profiles/default"),
        nix_link.clone(),
    ];
    envs.insert(
        "NIX_PROFILES".into(),
        nix_profiles
            .iter()
            .map(|path| path.as_os_str())
            .collect::<Vec<_>>()
            .join(OsStr::new(" ")),
    );

    {
        let mut xdg_data_dirs: Vec<PathBuf> = env_path("XDG_DATA_DIRS").unwrap_or_else(|| {
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share"),
            ]
        });

        xdg_data_dirs.extend(vec![
            nix_link.join("share"),
            PathBuf::from(LOCAL_STATE_DIR).join("nix/profiles/default/share"),
        ]);

        if let Ok(dirs) = env::join_paths(&xdg_data_dirs) {
            envs.insert("XDG_DATA_DIRS".into(), dirs);
        } else {
            return Err(Error::InvalidXdgDataDirs(xdg_data_dirs));
        }
    }

    if nonempty_var_os("NIX_SSL_CERT_FILE").is_none() {
        let mut candidate_locations = vec![
            PathBuf::from("/etc/ssl/certs/ca-certificates.crt"), // NixOS, Ubuntu, Debian, Gentoo, Arch
            PathBuf::from("/etc/ssl/ca-bundle.pem"),             // openSUSE Tumbleweed
            PathBuf::from("/etc/ssl/certs/ca-bundle.crt"),       // Old NixOS
            PathBuf::from("/etc/pki/tls/certs/ca-bundle.crt"),   // Fedora, CentOS
        ];

        // Add the various profiles, preferring the last profile, ie: most global profile (matches upstream behavior)
        for profile in nix_profiles.iter().rev() {
            candidate_locations.extend([
                profile.join("etc/ssl/certs/ca-bundle.crt"), // fall back to cacert in Nix profile
                profile.join("etc/ca-bundle.crt"),           // old cacert in Nix profile
            ]);
        }

        if let Some(cert) = candidate_locations.iter().find(|path| path.is_file()) {
            envs.insert("NIX_SSL_CERT_FILE".into(), cert.into());
        } else {
            tracing::warn!(
                "Could not identify any SSL certificates out of these candidates: {:?}",
                candidate_locations
            )
        }
    };

    {
        let mut path = vec![
            nix_link.join("bin"),
            // Note: This is typically only used in single-user installs, but I chose to do it in both for simplicity.
            // If there is good reason, we can make it fancier.
            PathBuf::from(LOCAL_STATE_DIR).join("nix/profiles/default/bin"),
        ];

        if let Some(old_path) = env_path("PATH") {
            path.extend(old_path);
        }

        if let Ok(dirs) = env::join_paths(&path) {
            envs.insert("PATH".into(), dirs);
        } else {
            return Err(Error::InvalidPathDirs(path));
        }
    }

    if let Some(old_path) = env_path("MANPATH") {
        let mut path = vec![
            nix_link.join("share/man"),
            // Note: This is typically only used in single-user installs, but I chose to do it in both for simplicity.
            // If there is good reason, we can make it fancier.
            PathBuf::from(LOCAL_STATE_DIR).join("nix/profiles/default/share/man"),
        ];

        path.extend(old_path);

        if let Ok(dirs) = env::join_paths(&path) {
            envs.insert("MANPATH".into(), dirs);
        } else {
            return Err(Error::InvalidManPathDirs(path));
        }
    }

    tracing::debug!("Calculated environment: {:#?}", envs);

    Ok(envs)
}