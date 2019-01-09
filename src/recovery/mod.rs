use clap::ArgMatches;
use disk_types::FileSystem;
use distinst::Disks;
use err_derive::Error;
use os_release::OsRelease;
use parallel_getter::ParallelGetter;
use std::fs::OpenOptions;
use std::io::{self, Write, Seek, SeekFrom};
use std::path::Path;
use std::path::PathBuf;
use sys_mount::{Mount, MountFlags, Unmount, UnmountFlags};
use tempfile::{tempdir, TempDir};

use ::release_api::{ApiError, Release};
use ::release_architecture::{detect_arch, ReleaseArchError};
use ::release_version::{detect_version, ReleaseVersionError};
use ::checksum::{ValidateError, validate_checksum};
use ::external::{findmnt_uuid, rsync};
use self::FileSystem::*;

pub type RecResult<T> = Result<T, RecoveryError>;

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error(display = "ISO does not exist at path")]
    IsoNotFound,
    #[error(display = "failed to fetch release data from server: {}", _0)]
    ApiError(ApiError),
    #[error(display = "failed to download ISO: {}", _0)]
    Download(Box<RecoveryError>),
    #[error(display = "failed to create temporary directory for ISO: {}", _0)]
    TempDir(io::Error),
    #[error(display = "I/O error: {}", _0)]
    Io(io::Error),
    #[error(display = "fetching from {} failed: {}", url, why)]
    Fetch { url: String, why: io::Error },
    #[error(display = "checksum for {:?} failed: {}", path, why)]
    Checksum { path: PathBuf, why: ValidateError },
    #[error(display = "recovery partition was not found")]
    RecoveryNotFound,
    #[error(display = "EFI partition was not found")]
    EfiNotFound,
    #[error(display = "failed to probe for recovery partition: {}", _0)]
    Probe(io::Error),
    #[error(display = "failed to fetch release architecture: {}", _0)]
    ReleaseArch(ReleaseArchError),
    #[error(display = "failed to fetch release versions: {}", _0)]
    ReleaseVersion(ReleaseVersionError)
}

impl From<io::Error> for RecoveryError {
    fn from(why: io::Error) -> Self {
        RecoveryError::Io(why)
    }
}

impl From<ReleaseVersionError> for RecoveryError {
    fn from(why: ReleaseVersionError) -> Self {
        RecoveryError::ReleaseVersion(why)
    }
}

impl From<ReleaseArchError> for RecoveryError {
    fn from(why: ReleaseArchError) -> Self {
        RecoveryError::ReleaseArch(why)
    }
}

pub fn recovery(matches: &ArgMatches) -> RecResult<()> {
    match matches.subcommand() {
        ("default-boot", Some(matches)) => {
            unimplemented!("default-boot is not implemented");
        }
        ("upgrade", Some(matches)) => {
            let result = Disks::probe_for(
                // Probe for a device which contains
                "recovery.conf",
                // Skip probing if a device is already mounted at
                "/recovery",
                // Only mount partitions with these file systems
                |fs| fs == Fat16 || fs == Fat32,
                // On finding the device, do the following to it:
                |device_mount_path| fetch_iso(matches, device_mount_path)
            );

            result.map_err(RecoveryError::Probe)?
                .map(|_| println!("upgrade of recovery partition was successful"))
        },
        _ => unreachable!()
    }
}

fn fetch_iso(matches: &ArgMatches, recovery_path: &Path) -> RecResult<()> {
    eprintln!("fetching ISO");
    if !recovery_path.exists() {
        return Err(RecoveryError::RecoveryNotFound);
    }

    let efi_path = Path::new("/boot/efi/EFI/");
    if !efi_path.exists() {
        return Err(RecoveryError::EfiNotFound);
    }

    let recovery_uuid = findmnt_uuid(recovery_path)?;
    let casper = ["casper-", &recovery_uuid].concat();
    let recovery = ["Recovery-", &recovery_uuid].concat();

    let mut temp_iso_dir = None;
    let iso = match matches.subcommand() {
        ("from-release", Some(matches)) => from_release(&mut temp_iso_dir, matches)?,
        ("from-file", Some(matches)) => from_file(matches)?,
        _ => unreachable!()
    };

    let tempdir = tempfile::tempdir().map_err(RecoveryError::TempDir)?;
    let _iso_mount = Mount::new(iso, tempdir.path(), "iso9660", MountFlags::RDONLY, None)?
        .into_unmount_drop(UnmountFlags::DETACH);

    let disk = tempdir.path().join(".disk");
    let dists = tempdir.path().join("dists");
    let pool = tempdir.path().join("pool");
    let casper_p = tempdir.path().join("casper/");
    let efi_recovery = efi_path.join(&recovery);
    let efi_initrd = efi_recovery.join("initrd.gz");
    let efi_vmlinuz = efi_recovery.join("vmlinuz.efi");
    let casper_initrd = recovery_path.join([&casper, "/initrd.gz"].concat());
    let casper_vmlinuz = recovery_path.join([&casper, "/vmlinuz.efi"].concat());
    let recovery_str = recovery_path.to_str().unwrap();

    rsync(
        &[&disk, &dists, &pool],
        recovery_str,
        &["-KLavc", "--inplace", "--delete"],
    )?;

    rsync(
        &[&casper_p],
        &[recovery_str, "/", &casper].concat(),
        &["-KLavc", "--inplace", "--delete"],
    )?;

    ::misc::cp(&casper_initrd, &efi_initrd)?;
    ::misc::cp(&casper_vmlinuz, &efi_vmlinuz)?;

    Ok(())
}

/// Fetches the release ISO remotely from api.pop-os.org.
fn from_release(temp: &mut Option<TempDir>, matches: &ArgMatches) -> RecResult<PathBuf> {
    let tmp_version: String;
    let version = match matches.value_of("VERSION") {
        Some(version) => version,
        None => {
            let (current, next) = detect_version()?;
            tmp_version = if matches.is_present("next") { next } else { current };
            &tmp_version
        }
    };

    let arch = match matches.value_of("ARCH") {
        Some(arch) => arch,
        None => detect_arch()?
    };

    let release = Release::get_release(version, arch).map_err(RecoveryError::ApiError)?;
    from_remote(temp, &release.url, &release.sha_sum)
        .map_err(|why| RecoveryError::Download(Box::new(why)))

}

/// Upgrades the recovery partition using an ISO that alreDy exists on the system.
fn from_file(matches: &ArgMatches) -> RecResult<PathBuf> {
    let path = matches.value_of("PATH").expect("missing reqired PATH argument");
    let path = PathBuf::from(path);
    if path.exists() {
        Ok(path)
    } else {
        Err(RecoveryError::IsoNotFound)
    }
}

/// Downloads the ISO from a remote location, to a temporary local directory.
///
/// Once downloaded, the ISO will be verfied against the given checksum.
fn from_remote(temp_dir: &mut Option<TempDir>, url: &str, checksum: &str) -> RecResult<PathBuf> {
    eprintln!("downloading ISO from remote at {}", url);
    let temp = tempdir().map_err(RecoveryError::TempDir)?;
    let path = temp.path().join("new.iso");

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(true)
        .open(&path)?;

    ParallelGetter::new(url, &mut file)
        .threads(8)
        .callback(1000, Box::new(|p, t| {
            println!("\rISO download: {} / {} MiB", p / 1024 / 1024, t / 1024 / 1024);
        }))
        .get()
        .map_err(|why| RecoveryError::Fetch {
            url: url.to_owned(),
            why,
        })?;

    file.flush()?;
    file.seek(SeekFrom::Start(0))?;

    validate_checksum(&mut file, checksum)
        .map_err(|why| RecoveryError::Checksum { path: path.clone(), why })?;

    *temp_dir = Some(temp);
    Ok(path)
}