use anyhow::{anyhow, Result};
use bzip2::read::BzDecoder;
use clap::{App, Arg, ArgGroup, ArgMatches};
use fern::colors::{Color, ColoredLevelConfig};
use fern::Dispatch;
use flate2::read::GzDecoder;
use log::{debug, error};
use octocrab::models::repos::Release;
use platforms::target::{TARGET_ARCH, TARGET_OS};
use regex::Regex;
use reqwest::StatusCode;
use std::env;
use std::fs::{create_dir_all, File};
use std::io::prelude::*;
use std::path::PathBuf;
use tar::Archive;
use tempfile::{tempdir, TempDir};
use xz::read::XzDecoder;
use zip::ZipArchive;
use zip_extensions::read::ZipArchiveExtensions;

#[cfg(target_family = "unix")]
use std::fs::{set_permissions, Permissions};
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let matches = app().get_matches();
    let res = init_logger(&matches);
    if let Err(e) = res {
        eprintln!("Error creating logger: {}", e);
        std::process::exit(126);
    }
    let u = UBI::new(&matches).await;
    let status = match u {
        Ok(u) => u.run().await,
        Err(e) => {
            debug!("{:#?}", e);
            error!("{}", e);
            127
        }
    };
    std::process::exit(status);
}

fn app<'a>() -> App<'a> {
    App::new("ubi")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Dave Rolsky <autarch@urth.org>")
        .about("The universal binary release installer")
        .arg(
            Arg::new("project")
                .long("project")
                .short('p')
                .takes_value(true)
                .required(true)
                .about(concat!(
                    "The project you want to install, like houseabsolute/precious",
                    " or https://github.com/houseabsolute/precious.",
                )),
        )
        .arg(
            Arg::new("tag")
                .long("tag")
                .short('t')
                .takes_value(true)
                .about("The tag to download. Defaults to the latest release."),
        )
        .arg(
            Arg::new("in")
                .long("in")
                .short('i')
                .takes_value(true)
                .about("The directory in which the binary should be placed. Defaults to ./bin."),
        )
        .arg(
            Arg::new("exe")
                .long("exe")
                .short('e')
                .takes_value(true)
                .about(concat!(
                    "The name of this project's executable. By default this is the same as the",
                    " project name, so for houseabsolute/precious we look for precious or",
                    r#" precious.exe. When running on Windows the ".exe" suffix will be added"#,
                    "as needed.",
                )),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .about("Enable verbose output"),
        )
        .arg(
            Arg::new("debug")
                .short('d')
                .long("debug")
                .about("Enable debugging output"),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .long("quiet")
                .about("Suppresses most output"),
        )
        .group(ArgGroup::new("log-level").args(&["verbose", "debug", "quiet"]))
}

pub fn init_logger(matches: &ArgMatches) -> Result<(), log::SetLoggerError> {
    let line_colors = ColoredLevelConfig::new()
        .error(Color::Red)
        .warn(Color::Yellow)
        .info(Color::BrightBlack)
        .debug(Color::BrightBlack);

    let level = if matches.is_present("debug") {
        log::LevelFilter::Debug
    } else if matches.is_present("verbose") {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Warn
    };

    let level_colors = line_colors.clone().info(Color::Green).debug(Color::Black);

    Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "{color_line}[{target}][{level}{color_line}] {message}\x1B[0m",
                color_line = format_args!(
                    "\x1B[{}m",
                    line_colors.get_color(&record.level()).to_fg_str()
                ),
                target = record.target(),
                level = level_colors.color(record.level()),
                message = message,
            ));
        })
        .level(level)
        // This is very noisy.
        .level_for("hyper", log::LevelFilter::Error)
        .chain(std::io::stderr())
        .apply()
}

#[derive(Debug)]
struct UBI {
    project_name: String,
    exe: String,
    install_path: PathBuf,
    release_info: Release,
    quiet: bool,
}

impl UBI {
    pub async fn new(matches: &ArgMatches) -> Result<UBI> {
        let project_name = Self::parse_project_name(matches.value_of("project"))?;
        let exe = Self::exe_name(matches, &project_name);
        let install_path = Self::install_path(matches, &exe)?;
        let release_info = Self::release_info(matches.value_of("tag"), &project_name).await?;
        Ok(UBI {
            project_name,
            exe,
            install_path,
            release_info,
            quiet: matches.is_present("quiet"),
        })
    }

    fn parse_project_name(project: Option<&str>) -> Result<String> {
        // We know this is Some because --project is required.
        let p = project.unwrap();
        let parts = p.split('/').collect::<Vec<&str>>();
        if parts.len() < 2 {
            return Err(anyhow!("could not parse project name from {}", p));
        }

        let org = parts[parts.len() - 2];
        let proj = parts[parts.len() - 1];
        debug!("Parsed project {} = {} / {}", p, org, proj);

        Ok(format!("{}/{}", org, proj))
    }

    fn exe_name(matches: &ArgMatches, project_name: &str) -> String {
        let exe = match matches.value_of("exe") {
            Some(e) => {
                if cfg!(windows) && !e.ends_with(".exe") {
                    format!("{}.exe", e)
                } else {
                    e.to_string()
                }
            }
            None => {
                let parts = project_name.split('/').collect::<Vec<&str>>();
                let e = parts[parts.len() - 1].to_string();
                if cfg!(windows) {
                    format!("{}.exe", e)
                } else {
                    e
                }
            }
        };
        debug!("exe name = {}", exe);
        exe
    }

    fn install_path(matches: &ArgMatches, exe: &str) -> Result<PathBuf> {
        let mut i = match matches.value_of("in") {
            Some(i) => PathBuf::from(i),
            None => {
                let mut i = env::current_dir()?;
                i.push("bin");
                i
            }
        };
        create_dir_all(&i)?;
        i.push(&exe);
        debug!("install path = {}", i.to_string_lossy());
        Ok(i)
    }

    async fn release_info(tag: Option<&str>, project_name: &str) -> Result<Release> {
        let mut parts = project_name.split('/');
        let o = parts.next().unwrap();
        let p = parts.next().unwrap();

        let octocrab = match env::var("GITHUB_TOKEN") {
            Ok(t) => {
                debug!("adding GitHub token to GitHub requests");
                octocrab::Octocrab::builder().personal_token(t).build()?
            }
            Err(_) => octocrab::Octocrab::builder().build()?,
        };

        let res = match tag {
            Some(t) => octocrab.repos(o, p).releases().get_by_tag(t).await,
            None => octocrab.repos(o, p).releases().get_latest().await,
        };
        match res {
            Ok(r) => {
                debug!("tag = {}", r.tag_name);
                Ok(r)
            }
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }

    async fn run(&self) -> i32 {
        match self.install_binary().await {
            Ok(()) => 0,
            Err(e) => {
                debug!("{:#?}", e);
                error!("{}", e);
                1
            }
        }
    }

    async fn install_binary(&self) -> Result<()> {
        let (_td1, archive_path) = self.download_release().await?;
        self.extract_binary(archive_path)?;
        self.make_binary_executable()?;

        Ok(())
    }

    async fn download_release(&self) -> Result<(TempDir, PathBuf)> {
        let archive_name = self.archive_name()?;
        debug!("downloading asset named {}", archive_name);

        let download_url = format!(
            "https://github.com/{}/releases/download/{}/{}",
            self.project_name, self.release_info.tag_name, archive_name,
        );
        let mut res = reqwest::get(&download_url).await?;
        if res.status() != StatusCode::OK {
            let mut msg = format!("error requesting {}: {}", download_url, res.status());
            if let Ok(t) = res.text().await {
                msg.push('\n');
                msg.push_str(&t);
            }
            return Err(anyhow!(msg));
        }

        let td = tempdir()?;
        let mut archive_path = td.path().to_owned();
        archive_path.push(archive_name);

        {
            let mut downloaded_file = File::create(&archive_path)?;
            while let Some(c) = res.chunk().await? {
                downloaded_file.write_all(c.as_ref())?;
            }
        }

        Ok((td, archive_path))
    }

    fn archive_name(&self) -> Result<String> {
        if self.release_info.assets.len() == 1 {
            return Ok(self.release_info.assets[0].name.clone());
        }

        let os_matcher = Self::os_matcher()?;
        debug!("matching binaries against OS using {}", os_matcher.as_str());

        let arch_matcher = Self::arch_matcher()?;
        debug!(
            "matching binaries against CPU architecture using {}",
            arch_matcher.as_str(),
        );

        let mut maybe: Vec<&str> = vec![];

        // This could all be done much more simply with the iterator's .find()
        // method, but then there's no place to put all the debugging output.
        for asset in &self.release_info.assets {
            debug!("matching against asset name = {}", asset.name);

            if asset.name.ends_with(".deb") {
                debug!("skipping deb {}", asset.name);
                continue;
            }
            if asset.name.ends_with(".rpm") {
                debug!("skipping rpm {}", asset.name);
                continue;
            }

            if os_matcher.is_match(&asset.name) {
                debug!("matches our OS");
                if arch_matcher.is_match(&asset.name) {
                    debug!("matches our CPU architecture");
                    maybe.push(&asset.name);
                } else {
                    debug!("does not match our CPU architecture");
                }
            } else {
                debug!("does not match our OS");
            }
        }

        if maybe.is_empty() {
            let assets = self
                .release_info
                .assets
                .iter()
                .map(|a| a.name.clone())
                .collect::<Vec<String>>()
                .join(", ");
            return Err(anyhow!(
                "could not find a release for this OS and architecture from {}",
                assets
            ));
        }

        let asset = self.pick_asset(maybe);
        debug!("picked asset named {}", asset);

        Ok(asset)
    }

    fn pick_asset(&self, maybe: Vec<&str>) -> String {
        if maybe.len() == 1 {
            debug!("only found one candidate asset");
            return maybe.first().unwrap().to_string();
        }

        if TARGET_ARCH.to_string().contains("64") {
            debug!(
                "found multiple installation candidates, looking for 64-bit binary first in {:?}",
                maybe,
            );
            if let Some(m) = maybe.iter().find(|&v| v.contains("64")) {
                return m.to_string();
            }
        }

        // I don't have any other criteria I could use to pick the right one.
        maybe.first().unwrap().to_string()
    }

    fn os_matcher() -> Result<Regex> {
        debug!("current OS = {}", TARGET_OS.as_str());

        // These values are those supported by Rust (based on the platforms
        // crate) and Go (based on
        // https://gist.github.com/asukakenji/f15ba7e588ac42795f421b48b8aede63).
        #[cfg(target_os = "linux")]
        return Regex::new(r"(?i:(?:\b|_)linux(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_os = "freebsd")]
        return Regex::new(r"(?i:(?:\b|_)freebsd(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_os = "openbsd")]
        return Regex::new(r"(?i:(?:\b|_)openbsd(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_os = "macos")]
        return Regex::new(r"(?i:(?:\b|_)(?:darwin|macos)(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_os = "windows")]
        return Regex::new(r"(?i:(?:\b|_)windows(?:\b|_))").map_err(anyhow::Error::new);

        #[allow(unreachable_code)]
        Err(anyhow!(
            "Cannot determine what type of compiled binary to use for this OS"
        ))
    }

    fn arch_matcher() -> Result<Regex> {
        debug!("current CPU architecture = {}", TARGET_ARCH.as_str());

        #[cfg(target_arch = "aarch64")]
        return Regex::new(r"(?i:(?:\b|_)aarch64(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "arm")]
        return Regex::new(r"(?i:(?:\b|_)arm(?:v[0-9]+|64)?(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "mips")]
        return Regex::new(r"(?i:(?:\b|_)mips(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "mips64")]
        return Regex::new(r"(?i:(?:\b|_)mips64(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "powerpc")]
        return Regex::new(r"(?i:(?:\b|_)(?:powerpc32(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "powerpc64")]
        return Regex::new(r"(?i:(?:\b|_)(?:powerpc64|ppc64(?:le|be)?)?)(?:\b|_))")
            .map_err(anyhow::Error::new);

        #[cfg(target_arch = "riscv")]
        return Regex::new(r"(?i:(?:\b|_)riscv(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "s390x")]
        return Regex::new(r"(?i:(?:\b|_)s390x(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "sparc")]
        return Regex::new(r"(?i:(?:\b|_)sparc(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "sparc64")]
        return Regex::new(r"(?i:(?:\b|_)sparc(?:64)?(?:\b|_))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "x86")]
        return Regex::new(r"(?i:(?:\b|_)(?:x86|386)(?:\b|_(?!64)))").map_err(anyhow::Error::new);

        #[cfg(target_arch = "x86_64")]
        return Regex::new(r"(?i:(?:\b|_)(?:x86|386|x86_64|amd64)(?:\b|_))")
            .map_err(anyhow::Error::new);

        #[allow(unreachable_code)]
        Err(anyhow!(
            "Cannot determine what type of compiled binary to use for this CPU architecture"
        ))
    }

    fn extract_binary(&self, downloaded_file: PathBuf) -> Result<()> {
        let filename = downloaded_file
            .file_name()
            .unwrap_or_else(|| {
                panic!(
                    "downloaded file path {} has no filename!",
                    downloaded_file.to_string_lossy()
                )
            })
            .to_string_lossy();
        if filename.ends_with(".tar.gz")
            || filename.ends_with(".tgz")
            || filename.ends_with(".tar.bz")
            || filename.ends_with(".tbz")
        {
            self.extract_tarball(downloaded_file)
        } else if filename.ends_with(".zip") {
            self.extract_zip(downloaded_file)
        } else if filename.ends_with(".gz") {
            self.ungzip(downloaded_file)
        } else {
            self.copy_executable(downloaded_file)
        }
    }

    fn extract_zip(&self, downloaded_file: PathBuf) -> Result<()> {
        debug!("extracting binary from zip file");

        let mut zip = ZipArchive::new(File::open(&downloaded_file)?)?;
        for i in 0..zip.len() {
            let path = PathBuf::from(zip.by_index(i).unwrap().name());
            if let Some(os_name) = path.file_name() {
                if let Some(n) = os_name.to_str() {
                    if n == self.exe {
                        debug!(
                            "extracting zip file member to {}",
                            self.install_path.to_string_lossy(),
                        );
                        let res = zip.extract_file(i, &self.install_path, true);
                        return match res {
                            Ok(_) => Ok(()),
                            Err(e) => Err(anyhow::Error::new(e)),
                        };
                    }
                }
            }
        }

        debug!("could not find any entries named {}", self.exe);
        Err(anyhow!(
            "could not find any files named {} in the downloaded zip file",
            self.exe,
        ))
    }

    fn extract_tarball(&self, downloaded_file: PathBuf) -> Result<()> {
        debug!(
            "extracting binary from tarball at {}",
            downloaded_file.to_string_lossy(),
        );

        let mut arch = Self::tar_reader_for(downloaded_file)?;
        for entry in arch.entries()? {
            let mut entry = entry?;
            let path = entry.path()?;
            debug!("found tarball entry with path {}", path.to_string_lossy());
            if let Some(os_name) = path.file_name() {
                if let Some(n) = os_name.to_str() {
                    if n == self.exe {
                        debug!(
                            "extracting tarball entry to {}",
                            self.install_path.to_string_lossy(),
                        );
                        return match entry.unpack(&self.install_path) {
                            Ok(_) => Ok(()),
                            Err(e) => Err(anyhow::Error::new(e)),
                        };
                    }
                }
            }
        }

        debug!("could not find any entries named {}", self.exe);
        Err(anyhow!(
            "could not find any files named {} in the downloaded tarball",
            self.exe,
        ))
    }

    fn tar_reader_for(downloaded_file: PathBuf) -> Result<Archive<Box<dyn Read>>> {
        let file = File::open(&downloaded_file)?;

        let ext = downloaded_file.extension();
        match ext {
            Some(ext) => match ext.to_str() {
                Some("bz") | Some("tbz") => Ok(Archive::new(Box::new(BzDecoder::new(file)))),
                Some("gz") | Some("tgz") => Ok(Archive::new(Box::new(GzDecoder::new(file)))),
                Some("xz") | Some("txz") => Ok(Archive::new(Box::new(XzDecoder::new(file)))),
                Some(e) => Err(anyhow!(
                    "don't know how to uncompress a tarball with extension = {}",
                    e,
                )),
                None => Err(anyhow!(
                    "tarball {:?} has a non-UTF-8 extension",
                    downloaded_file,
                )),
            },
            None => Ok(Archive::new(Box::new(file))),
        }
    }

    fn ungzip(&self, downloaded_file: PathBuf) -> Result<()> {
        debug!("uncompressing binary from gzip file");

        let mut reader = GzDecoder::new(File::open(&downloaded_file)?);
        let mut writer = File::create(&self.install_path)?;
        std::io::copy(&mut reader, &mut writer)?;

        Ok(())
    }

    fn copy_executable(&self, exe_file: PathBuf) -> Result<()> {
        debug!("copying binary to final location");
        std::fs::copy(&exe_file, &self.install_path)?;

        Ok(())
    }

    fn make_binary_executable(&self) -> Result<()> {
        #[cfg(target_family = "windows")]
        return Ok(());

        #[cfg(target_family = "unix")]
        match set_permissions(&self.install_path, Permissions::from_mode(0o755)) {
            Ok(_) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}
