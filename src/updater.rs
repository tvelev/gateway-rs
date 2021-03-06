use crate::{result::Result, settings::Settings};
use log::{info, warn};
use semver::Version;
use serde::{de, Deserialize, Deserializer};
use std::{
    env, fmt, io,
    path::{Path, PathBuf},
    process,
};
use tokio::time;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Channel {
    Alpha,
    Beta,
    Release,
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Channel::Alpha => "alpha",
            Channel::Beta => "beta",
            Channel::Release => "release",
        };
        f.write_str(s)
    }
}

#[derive(Debug)]
pub struct Updater {
    enabled: bool,
    client: reqwest::Client,
    url: reqwest::Url,
    channel: Channel,
    platform: String,
    interval: time::Duration,
    install_command: String,
}

static USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

impl Updater {
    pub fn new(settings: &Settings) -> Result<Self> {
        use reqwest::header;
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github.v3+json"),
        );
        let builder = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers);
        Ok(Self {
            enabled: settings.update.enabled,
            client: builder.build()?,
            channel: settings.update.channel.clone(),
            platform: settings.update.platform.clone(),
            interval: time::Duration::from_secs(settings.update.interval as u64 * 60),
            url: settings.update.url.clone(),
            install_command: settings.update.command.clone(),
        })
    }

    pub async fn run(&self, shutdown: triggered::Listener) -> Result {
        if !self.enabled {
            info!("disabling updater");
            return Ok(());
        }
        info!("starting updater");
        let mut interval = time::interval(self.interval);
        loop {
            tokio::select! {
                _ = shutdown.clone() => {
                    info!("shutting down");
                    return Ok(())
                },
                _ = interval.tick() => {
                    // Get teh current cersion and find teh first replease
                    // version in the settings channel that is newer than the
                    // package version.
                    let current_version = Version::parse(env!("CARGO_PKG_VERSION")).expect("semver package version");
                    let mut release_list = ReleaseList::new(self.client.clone(), self.url.clone());
                    match release_list.first(|r| {
                        r.in_channel(&self.channel) && r.version > current_version
                    }).await {
                        Ok(Some(release)) => {
                            let package_name = format!("helium-gateway-v{}-{}.ipk",
                                                       release.version.to_string(),
                                                       self.platform);
                            // Check for an asset given teh assumed name for the package
                            match release.asset_named(&package_name) {
                                Some(asset) => {
                                    info!("downloading update {:?}", package_name);
                                    let download_path = self.download_path(&package_name);
                                    asset.download(&download_path).await?;
                                    info!("installing update {:?}", package_name);
                                    return self.install(&download_path).await;
                                },
                                None => warn!("no release asset found for {}", package_name)
                            }
                        },
                        Ok(None) => info!("no update found"),
                        Err(err) => warn!("failed to fetch releases: {:?}", err)
                    }
                }
            }
        }
    }

    /// Returns a temporary location to download a package into. Do _not_ return a
    /// path that will be used for an actual update since a partial download may
    /// remain after download failures.
    pub fn download_path(&self, package_name: &str) -> PathBuf {
        env::temp_dir().join(package_name)
    }

    /// Does a platform specific install of the given package. Some platform
    /// will mvove the package into a staging location and reboot to trigger the
    /// install whereas others may just need a package install and service
    /// restart.
    pub async fn install(&self, download_path: &Path) -> Result {
        match process::Command::new(&self.install_command)
            .arg(download_path)
            .output()
        {
            Ok(output) => {
                if output.status.success() {
                    return Ok(());
                }
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    String::from_utf8(output.stderr).unwrap(),
                )
                .into())
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// Represeents a versioned release  with one or more assets
#[derive(Debug, Deserialize)]
pub struct Release {
    /// The version of the release
    #[serde(deserialize_with = "deserialize_version", rename = "tag_name")]
    version: Version,
    /// The list of assets for the release
    assets: Vec<ReleaseAsset>,
}

fn deserialize_version<'de, D>(d: D) -> std::result::Result<Version, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    let version_str = match s.strip_prefix("v") {
        Some(v) => v,
        None => &s,
    };
    version_str
        .parse()
        .map_err(|e| de::Error::custom(format!("invalid release format \"{}\": {}", s, e)))
}

impl Release {
    /// Checks whether a release is in the given channel. For the release
    /// channel any non prerelease version is considered good. For alpha/beta
    /// the alpha or beta strings have to be part of the "pre" release
    /// identifiers of the version.
    pub fn in_channel(&self, channel: &Channel) -> bool {
        match channel {
            Channel::Release => !self.version.is_prerelease(),
            Channel::Alpha | Channel::Beta => {
                use semver::Identifier;
                let tag = channel.to_string();
                for identifier in &self.version.pre {
                    if let Identifier::AlphaNumeric(v) = identifier {
                        if v.contains(&tag) {
                            return true;
                        }
                    }
                }
                false
            }
        }
    }

    /// Find an asset with a given name in this release. Returns None if no such
    /// asset was found.
    pub fn asset_named(&self, name: &str) -> Option<&ReleaseAsset> {
        for asset in &self.assets {
            if asset.name == name {
                return Some(&asset);
            }
        }
        None
    }
}

/// A release asset is a named, downloadable file that can be installed on a
/// system.
#[derive(Debug, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    #[serde(rename = "browser_download_url")]
    pub download_url: String,
    pub size: usize,
}

impl ReleaseAsset {
    /// Downloads the asset to a given destination.
    pub async fn download(&self, dest: &Path) -> Result {
        use tokio::{fs::File, io::BufWriter, prelude::*};
        let client = reqwest::Client::new();
        match client.get(&self.download_url).send().await {
            Ok(mut response) => {
                let file = File::create(&dest).await?;
                let mut writer = BufWriter::new(file);
                let mut bytes_written = 0;
                while let Some(chunk) = response.chunk().await? {
                    writer.write_all(&chunk).await?;
                    bytes_written += chunk.len();
                }
                writer.flush().await?;
                if bytes_written != self.size {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!(
                            "expected {} download bytes, but got {}",
                            self.size, bytes_written
                        ),
                    )
                    .into());
                }
                Ok(())
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// A release list represents a list of Github releases. The release list is
/// lazy and will request the API for more releases as they are requested.
#[derive(Debug)]
pub struct ReleaseList {
    client: reqwest::Client,
    url: reqwest::Url,
    next_page: u8,
    current_page: Vec<Release>,
    finished: bool,
}

const GH_PAGE_SIZE: u8 = 10;

impl ReleaseList {
    /// Creates a new release list given a request client.
    pub fn new(client: reqwest::Client, url: reqwest::Url) -> Self {
        Self {
            client,
            url,
            next_page: 1,
            current_page: vec![],
            finished: false,
        }
    }

    /// Returns the first release that matches the given filter. Returns None if
    /// the filter never returns true and the release list is exhausted.
    pub async fn first<F>(&mut self, needle: F) -> Result<Option<Release>>
    where
        F: Fn(&Release) -> bool,
    {
        while let Some(release) = self.next().await? {
            if needle(&release) {
                return Ok(Some(release));
            }
        }
        Ok(None)
    }

    /// Fetches the next release from the release list. Returns None if no more releases are available.
    pub async fn next(&mut self) -> Result<Option<Release>> {
        if self.finished {
            return Ok(None);
        };
        if self.current_page.is_empty() {
            let mut next_page = self
                .client
                .get(self.url.clone())
                .query(&[("per_page", GH_PAGE_SIZE), ("page", self.next_page)])
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Release>>()
                .await?;
            self.finished = next_page.len() < GH_PAGE_SIZE as usize;
            next_page.reverse();
            self.current_page = next_page;
            self.next_page += 1;
        };
        Ok(self.current_page.pop())
    }
}
