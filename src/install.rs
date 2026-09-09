use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const GITHUB_API: &str = "https://api.github.com/repos/denoland/deno/releases/latest";
const USER_AGENT: &str = concat!("scriptmcp/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenoSource {
    Sidecar,
    Path,
    Explicit,
}

impl DenoSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Sidecar => "next to ScriptMCP",
            Self::Path => "on PATH",
            Self::Explicit => "from --deno",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DetectedDeno {
    pub path: PathBuf,
    pub version: String,
    pub source: DenoSource,
}

#[derive(Debug, Clone)]
pub enum InstallProgress {
    Message(String),
    Download { done: u64, total: Option<u64> },
}

#[derive(Debug, Clone, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
}

pub fn deno_binary_name() -> &'static str {
    if cfg!(windows) {
        "deno.exe"
    } else {
        "deno"
    }
}

pub fn executable_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to locate this executable")?;
    exe.parent()
        .map(Path::to_path_buf)
        .context("executable has no parent directory")
}

pub fn sidecar_path() -> Result<PathBuf> {
    Ok(sidecar_path_in(&executable_dir()?))
}

pub fn sidecar_path_in(dir: &Path) -> PathBuf {
    dir.join(deno_binary_name())
}

pub fn deno_asset_name() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("deno-x86_64-unknown-linux-gnu.zip"),
        ("linux", "aarch64") => Ok("deno-aarch64-unknown-linux-gnu.zip"),
        ("macos", "x86_64") => Ok("deno-x86_64-apple-darwin.zip"),
        ("macos", "aarch64") => Ok("deno-aarch64-apple-darwin.zip"),
        ("windows", "x86_64") => Ok("deno-x86_64-pc-windows-msvc.zip"),
        ("windows", "aarch64") => Ok("deno-aarch64-pc-windows-msvc.zip"),
        (os, arch) => bail!("no official Deno build for {os}/{arch}"),
    }
}

pub fn is_custom_deno_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name != "deno" && name != "deno.exe" {
        return true;
    }
    path.is_absolute() || path.components().count() > 1
}

pub fn resolve_deno(explicit: &Path) -> PathBuf {
    detect(explicit)
        .map(|found| found.path)
        .unwrap_or_else(|| explicit.to_path_buf())
}

pub fn detect(explicit: &Path) -> Option<DetectedDeno> {
    let mut candidates = Vec::new();
    if is_custom_deno_path(explicit) {
        candidates.push((explicit.to_path_buf(), DenoSource::Explicit));
    } else {
        if let Ok(sidecar) = sidecar_path() {
            candidates.push((sidecar, DenoSource::Sidecar));
        }
        candidates.push((explicit.to_path_buf(), DenoSource::Path));
        if explicit != Path::new("deno") && explicit != Path::new("deno.exe") {
            candidates.push((PathBuf::from(deno_binary_name()), DenoSource::Path));
        }
    }

    for (path, source) in candidates {
        if let Ok(version) = probe_version(&path) {
            return Some(DetectedDeno {
                path,
                version,
                source,
            });
        }
    }
    None
}

pub fn probe_version(path: &Path) -> Result<String> {
    let output = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to execute {}", path.display()))?;
    if !output.status.success() {
        bail!(
            "`{} --version` failed: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("deno").trim().to_string();
    Ok(line)
}

pub fn install_sidecar(mut progress: impl FnMut(InstallProgress)) -> Result<DetectedDeno> {
    let dest_dir = executable_dir()?;
    let dest = sidecar_path_in(&dest_dir);
    install_into(&dest_dir, &dest, &mut progress)?;
    let version = probe_version(&dest)?;
    Ok(DetectedDeno {
        path: dest,
        version,
        source: DenoSource::Sidecar,
    })
}

fn install_into(
    dest_dir: &Path,
    dest: &Path,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<()> {
    fs::create_dir_all(dest_dir)
        .with_context(|| format!("cannot write to {}", dest_dir.display()))?;

    let client = http_client()?;
    progress(InstallProgress::Message(
        "Looking up the latest Deno release…".into(),
    ));
    let (tag, asset) = latest_asset(&client)?;
    progress(InstallProgress::Message(format!(
        "Downloading Deno {tag} ({})…",
        asset.name
    )));

    let zip_path = dest_dir.join("deno.zip.partial");
    download(&client, &asset.browser_download_url, &zip_path, progress)
        .with_context(|| format!("failed to download {}", asset.browser_download_url))?;

    progress(InstallProgress::Message("Verifying checksum…".into()));
    let expected = expected_sha256(&client, &asset)?;
    let actual = sha256_file(&zip_path)?;
    if actual != expected {
        let _ = fs::remove_file(&zip_path);
        bail!("Deno download checksum mismatch (expected {expected}, got {actual})");
    }

    progress(InstallProgress::Message(format!(
        "Installing to {}…",
        dest.display()
    )));
    extract_deno_binary(&zip_path, dest)?;
    let _ = fs::remove_file(&zip_path);
    Ok(())
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .context("failed to build HTTP client")
}

fn latest_asset(client: &reqwest::blocking::Client) -> Result<(String, Asset)> {
    let release: Release = client
        .get(GITHUB_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("failed to query GitHub for the latest Deno release")?
        .error_for_status()
        .context("GitHub rejected the Deno release lookup")?
        .json()
        .context("failed to parse Deno release metadata")?;
    let wanted = deno_asset_name()?;
    let asset = pick_asset(&release, wanted)?;
    Ok((release.tag_name, asset))
}

fn pick_asset(release: &Release, wanted: &str) -> Result<Asset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == wanted)
        .cloned()
        .with_context(|| {
            format!(
                "Deno release {} has no asset named {wanted}",
                release.tag_name
            )
        })
}

fn expected_sha256(client: &reqwest::blocking::Client, asset: &Asset) -> Result<String> {
    if let Some(digest) = asset.digest.as_deref() {
        if let Some(hash) = digest.strip_prefix("sha256:") {
            let hash = hash.trim().to_ascii_lowercase();
            if is_sha256_hex(&hash) {
                return Ok(hash);
            }
        }
    }

    let url = format!("{}.sha256sum", asset.browser_download_url);
    let text = client
        .get(&url)
        .send()
        .with_context(|| format!("failed to download checksum from {url}"))?
        .error_for_status()
        .with_context(|| format!("checksum file missing at {url}"))?
        .text()
        .context("failed to read checksum file")?;
    parse_sha256sum(&text)
}

fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
    progress: &mut impl FnMut(InstallProgress),
) -> Result<()> {
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("failed to GET {url}"))?
        .error_for_status()
        .with_context(|| format!("download failed for {url}"))?;
    let total = response.content_length();
    let mut file =
        File::create(dest).with_context(|| format!("failed to create {}", dest.display()))?;
    let mut done = 0_u64;
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = io::Read::read(&mut response, &mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        progress(InstallProgress::Download { done, total });
    }
    file.sync_all()?;
    Ok(())
}

fn extract_deno_binary(zip_path: &Path, dest: &Path) -> Result<()> {
    let file =
        File::open(zip_path).with_context(|| format!("failed to open {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("invalid zip at {}", zip_path.display()))?;

    let mut index = None;
    for i in 0..archive.len() {
        let entry = archive.by_index(i)?;
        let name = entry.name().replace('\\', "/");
        let base = name.rsplit('/').next().unwrap_or("");
        if base == "deno" || base == "deno.exe" {
            index = Some(i);
            break;
        }
    }
    let index = index.context("the Deno archive did not contain a deno binary")?;
    let mut src = archive.by_index(index)?;
    let tmp = dest.with_extension("partial");
    let mut out =
        File::create(&tmp).with_context(|| format!("failed to create {}", tmp.display()))?;
    io::copy(&mut src, &mut out)?;
    out.sync_all()?;
    drop(out);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to mark {} executable", tmp.display()))?;
    }

    fs::rename(&tmp, dest)
        .with_context(|| format!("failed to install Deno to {}", dest.display()))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let n = io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.chars().all(|ch| ch.is_ascii_hexdigit())
}

pub fn parse_sha256sum(text: &str) -> Result<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Hash") {
            let hash = rest.trim().trim_start_matches(':').trim();
            if is_sha256_hex(hash) {
                return Ok(hash.to_ascii_lowercase());
            }
        }
    }
    for token in text.split_whitespace() {
        if is_sha256_hex(token) {
            return Ok(token.to_ascii_lowercase());
        }
    }
    bail!("could not parse Deno sha256sum file");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gnu_and_powershell_checksums() {
        let gnu = "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd  deno.zip\n";
        assert_eq!(
            parse_sha256sum(gnu).unwrap(),
            "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );

        let powershell = "Algorithm : SHA256\nHash      : ABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCDEFABCD\n";
        assert_eq!(
            parse_sha256sum(powershell).unwrap(),
            "abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );
    }

    #[test]
    fn picks_matching_release_asset() {
        let release = Release {
            tag_name: "v2.8.0".into(),
            assets: vec![
                Asset {
                    name: "deno-x86_64-unknown-linux-gnu.zip".into(),
                    browser_download_url: "https://example.invalid/deno.zip".into(),
                    digest: Some(
                        "sha256:abcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcdefabcd"
                            .into(),
                    ),
                },
                Asset {
                    name: "notes.txt".into(),
                    browser_download_url: "https://example.invalid/notes".into(),
                    digest: None,
                },
            ],
        };
        let asset = pick_asset(&release, "deno-x86_64-unknown-linux-gnu.zip").unwrap();
        assert_eq!(
            asset.browser_download_url,
            "https://example.invalid/deno.zip"
        );
    }

    #[test]
    fn sidecar_lives_in_given_dir() {
        let path = sidecar_path_in(Path::new("/opt/scriptmcp"));
        assert!(path.ends_with(deno_binary_name()));
        assert_eq!(path.parent().unwrap(), Path::new("/opt/scriptmcp"));
    }

    #[test]
    fn custom_path_detection() {
        assert!(is_custom_deno_path(Path::new("/usr/bin/deno")));
        assert!(is_custom_deno_path(Path::new("./bin/deno")));
        assert!(!is_custom_deno_path(Path::new("deno")));
    }
}
