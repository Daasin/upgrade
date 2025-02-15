use crate::release_api::{ApiError, Release};
use anyhow::Context;
use ubuntu_version::{Version, VersionError};

#[derive(Debug)]
pub enum BuildStatus {
    Blacklisted,
    Build(u16),
    ConnectionIssue(isahc::Error),
    InternalIssue(ApiError),
    ServerStatus(isahc::http::StatusCode),
}

impl BuildStatus {
    pub fn is_ok(&self) -> bool {
        if let BuildStatus::Build(_) = *self {
            true
        } else {
            false
        }
    }

    pub fn status_code(&self) -> i16 {
        match *self {
            BuildStatus::ConnectionIssue(_) => -3,
            BuildStatus::ServerStatus(_) => -2,
            BuildStatus::InternalIssue(_) => -1,
            BuildStatus::Build(build) => build as i16,
            BuildStatus::Blacklisted => -4,
        }
    }
}

impl From<Result<u16, ApiError>> for BuildStatus {
    fn from(result: Result<u16, ApiError>) -> Self {
        match result {
            Err(ApiError::Get(why)) => BuildStatus::ConnectionIssue(why),
            Err(ApiError::Status(why)) => BuildStatus::ServerStatus(why),
            Err(otherwise) => BuildStatus::InternalIssue(otherwise),
            Ok(build) => BuildStatus::Build(build),
        }
    }
}

impl PartialEq for BuildStatus {
    fn eq(&self, other: &BuildStatus) -> bool {
        match (self, other) {
            (BuildStatus::Blacklisted, BuildStatus::Blacklisted)
            | (BuildStatus::ConnectionIssue(_), BuildStatus::ConnectionIssue(_))
            | (BuildStatus::InternalIssue(_), BuildStatus::InternalIssue(_))
            | (BuildStatus::ServerStatus(_), BuildStatus::ServerStatus(_)) => true,
            (BuildStatus::Build(a), BuildStatus::Build(b)) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct ReleaseStatus {
    pub current: &'static str,
    pub next:    &'static str,
    pub build:   BuildStatus,
    pub is_lts:  bool,
}

impl ReleaseStatus {
    pub fn is_lts(&self) -> bool { self.is_lts }
}

pub fn next(development: bool) -> Result<ReleaseStatus, VersionError> {
    Version::detect().map(|current| {
        next_(current, development, |build| Release::build_exists(build, "intel").into())
    })
}

pub fn current(version: Option<&str>) -> anyhow::Result<(Box<str>, u16)> {
    info!("Checking for current release of {:?}", version);

    if let Some(version) = version {
        let build = Release::build_exists(version, "intel")
            .with_context(|| fomat!("failed to find build for "(version)))?;

        return Ok((version.into(), build));
    }

    let current = Version::detect().context("cannot detect current version of Pop")?;
    let release_str = release_str(current.major, current.minor);

    let build = Release::build_exists(release_str, "intel")
        .with_context(|| fomat!("failed to find build for "(release_str)))?;

    Ok((release_str.into(), build))
}

pub fn release_str(major: u8, minor: u8) -> &'static str {
    match (major, minor) {
        (18, 4) => "18.04",
        (19, 10) => "18.10",
        (20, 4) => "20.04",
        (20, 10) => "20.10",
        (21, 4) => "21.04",
        _ => panic!("this version of pop-upgrade is not supported on this release"),
    }
}

fn next_(
    current: Version,
    development: bool,
    release_check: impl Fn(&str) -> BuildStatus,
) -> ReleaseStatus {
    let next: &str;
    match (current.major, current.minor) {
        (18, 4) => {
            // next = if development { "20.10" } else { "20.04" };
            next = "20.04";

            ReleaseStatus { build: release_check(next), current: "18.04", is_lts: true, next }
        }

        (19, 10) => {
            next = "20.04";

            ReleaseStatus { build: release_check(next), current: "19.10", is_lts: false, next }
        }

        (20, 4) => {
            next = "20.10";

            ReleaseStatus { build: release_check(next), current: "20.04", is_lts: true, next }
        }

        (20, 10) => {
            next = "21.04";

            ReleaseStatus {
                build: if development { release_check(next) } else { BuildStatus::Blacklisted },
                current: "20.10",
                is_lts: false,
                next,
            }
        }

        (21, 4) => ReleaseStatus {
            build:   BuildStatus::Blacklisted,
            current: "21.04",
            is_lts:  false,
            next:    "21.10",
        },

        _ => panic!("this version of pop-upgrade is not supported on this release"),
    }
}
